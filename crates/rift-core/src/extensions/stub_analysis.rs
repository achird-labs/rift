//! Stub analysis for detecting conflicts, overlaps, and potential issues.
//!
//! This module provides Rift extensions for analyzing stubs that go beyond
//! Mountebank compatibility:
//!
//! - Duplicate ID detection
//! - Predicate overlap analysis
//! - Shadowed stub warnings
//!
//! **Mountebank Behavioral Note**: Mountebank does NOT provide any overlap
//! detection or warnings. It silently uses first-match-wins semantics.
//! These features are Rift extensions for improved developer experience.

use crate::imposter::Stub;
use crate::imposter::{Predicate, PredicateOperation};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Cap on the number of stub-analysis warnings retained in a single result (issue #423). Beyond
/// this, a single [`WarningType::Truncated`] summary records how many were suppressed, so a
/// pathological config (thousands of overlapping stubs) can't allocate unbounded memory.
pub const MAX_STUB_WARNINGS: usize = 100;

/// Above this stub count the O(n²) subset-shadowing heuristic is skipped (issue #423): it is
/// advisory only, and quadratic pairwise comparison is not worth its cost on large imposters.
/// Exact-duplicate detection stays O(n) (hash-based) at any size.
const SHADOW_HEURISTIC_MAX_STUBS: usize = 200;

/// Warning types for stub analysis (Rift extension)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StubWarning {
    /// Type of warning
    pub warning_type: WarningType,
    /// Human-readable message
    pub message: String,
    /// Index of the affected stub (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_index: Option<usize>,
    /// ID of the affected stub (if applicable)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<String>,
    /// Index of the shadowing stub (for shadow warnings)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadowed_by_index: Option<usize>,
}

/// Types of warnings that can be generated
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WarningType {
    /// Stub with same ID already exists
    DuplicateId,
    /// Stub predicates are identical to another stub
    ExactDuplicate,
    /// Stub may be shadowed by an earlier stub with overlapping predicates
    PotentiallyShadowed,
    /// Stub has empty predicates (matches all requests)
    CatchAll,
    /// Catch-all stub is not at the end of the list
    CatchAllNotLast,
    /// Analysis produced more warnings than the retained cap; the summary records how many were
    /// suppressed (issue #423).
    Truncated,
}

/// Result of stub analysis
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StubAnalysisResult {
    /// Warnings generated during analysis
    pub warnings: Vec<StubWarning>,
}

impl StubAnalysisResult {
    pub fn new() -> Self {
        Self { warnings: vec![] }
    }

    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    pub fn add_warning(&mut self, warning: StubWarning) {
        self.warnings.push(warning);
    }
}

/// Analyzes stubs for potential issues like duplicates, overlaps, and shadowing.
///
/// This is a Rift extension - Mountebank does not provide this functionality.
pub fn analyze_stubs(stubs: &[Stub]) -> StubAnalysisResult {
    let mut result = StubAnalysisResult::new();
    // Count of every warning the analysis *would* emit; `result.warnings` retains at most
    // MAX_STUB_WARNINGS of them, and the gap becomes the Truncated summary (issue #423).
    let mut total: usize = 0;
    let mut push = |warnings: &mut Vec<StubWarning>, w: StubWarning| {
        total += 1;
        if warnings.len() < MAX_STUB_WARNINGS {
            warnings.push(w);
        }
    };

    let mut seen_ids: HashMap<String, usize> = HashMap::new();
    // Canonical predicate-set key -> first stub index carrying it. Exact-duplicate detection is
    // O(n) instead of the old O(n²) pairwise scan (issue #423): the key encodes exactly what
    // `predicates_equal` compares — the predicate count plus the order-independent, de-duplicated
    // set of canonicalized predicates — so a hash hit means the same match set.
    let mut seen_predicates: HashMap<String, usize> = HashMap::new();
    // Index of the first catch-all (empty-predicate) stub seen so far.
    let mut first_catch_all: Option<usize> = None;
    // The subset-shadowing heuristic is the only remaining quadratic scan; gate it by size.
    let run_shadow_heuristic = stubs.len() <= SHADOW_HEURISTIC_MAX_STUBS;

    for (index, stub) in stubs.iter().enumerate() {
        // Duplicate IDs.
        if let Some(id) = &stub.id {
            if let Some(&existing_index) = seen_ids.get(id) {
                push(
                    &mut result.warnings,
                    StubWarning {
                        warning_type: WarningType::DuplicateId,
                        message: format!(
                            "Stub at index {index} has duplicate ID '{id}' (same as stub at index {existing_index})"
                        ),
                        stub_index: Some(index),
                        stub_id: Some(id.clone()),
                        shadowed_by_index: Some(existing_index),
                    },
                );
            } else {
                seen_ids.insert(id.clone(), index);
            }
        }

        // Catch-all (empty predicates).
        if stub.predicates.is_empty() {
            if first_catch_all.is_none() {
                first_catch_all = Some(index);
            }
            push(
                &mut result.warnings,
                StubWarning {
                    warning_type: WarningType::CatchAll,
                    message: format!(
                        "Stub at index {index} has empty predicates and will match ALL requests"
                    ),
                    stub_index: Some(index),
                    stub_id: stub.id.clone(),
                    shadowed_by_index: None,
                },
            );
        }

        // Exact predicate duplicates — O(1) hash lookup against the first stub with this key.
        let key = predicate_key(&stub.predicates);
        match seen_predicates.get(&key) {
            Some(&first_index) => push(
                &mut result.warnings,
                StubWarning {
                    warning_type: WarningType::ExactDuplicate,
                    message: format!(
                        "Stub at index {index} has identical predicates to stub at index {first_index} and will never match"
                    ),
                    stub_index: Some(index),
                    stub_id: stub.id.clone(),
                    shadowed_by_index: Some(first_index),
                },
            ),
            None => {
                seen_predicates.insert(key, index);
            }
        }

        // Potential shadowing of a specific (non-empty) stub by an earlier one.
        if !stub.predicates.is_empty() {
            // Any earlier catch-all shadows this stub — O(1) via the first-catch-all index.
            if let Some(catch_all_index) = first_catch_all {
                push(
                    &mut result.warnings,
                    StubWarning {
                        warning_type: WarningType::PotentiallyShadowed,
                        message: format!(
                            "Stub at index {index} may be shadowed by catch-all stub at index {catch_all_index}"
                        ),
                        stub_index: Some(index),
                        stub_id: stub.id.clone(),
                        shadowed_by_index: Some(catch_all_index),
                    },
                );
            }
            // Subset-overlap heuristic — the remaining O(n²) scan, skipped on large imposters.
            if run_shadow_heuristic {
                for (earlier_index, earlier_stub) in stubs[..index].iter().enumerate() {
                    if !earlier_stub.predicates.is_empty()
                        && is_subset_predicates(&stub.predicates, &earlier_stub.predicates)
                    {
                        push(
                            &mut result.warnings,
                            StubWarning {
                                warning_type: WarningType::PotentiallyShadowed,
                                message: format!(
                                    "Stub at index {index} may be partially shadowed by stub at index {earlier_index} which has overlapping predicates"
                                ),
                                stub_index: Some(index),
                                stub_id: stub.id.clone(),
                                shadowed_by_index: Some(earlier_index),
                            },
                        );
                    }
                }
            }
        }
    }

    // Warn if a catch-all is not at the end.
    if let Some(catch_all_idx) = first_catch_all
        && catch_all_idx < stubs.len() - 1
    {
        push(
            &mut result.warnings,
            StubWarning {
                warning_type: WarningType::CatchAllNotLast,
                message: format!(
                    "Catch-all stub at index {} will shadow {} stub(s) after it",
                    catch_all_idx,
                    stubs.len() - catch_all_idx - 1
                ),
                stub_index: Some(catch_all_idx),
                stub_id: stubs[catch_all_idx].id.clone(),
                shadowed_by_index: None,
            },
        );
    }

    // Record how many warnings were suppressed by the cap rather than silently dropping them.
    let retained = result.warnings.len();
    if total > retained {
        result.warnings.push(StubWarning {
            warning_type: WarningType::Truncated,
            message: format!(
                "{} additional stub warning(s) suppressed (showing first {retained})",
                total - retained
            ),
            stub_index: None,
            stub_id: None,
            shadowed_by_index: None,
        });
    }

    result
}

/// Canonical key for a predicate list that matches [`predicates_equal`] semantics: two lists share
/// a key iff they have the same length and the same order-independent set of canonicalized
/// predicates. Used for O(n) exact-duplicate detection (issue #423).
fn predicate_key(predicates: &[Predicate]) -> String {
    let mut set: Vec<String> = predicates
        .iter()
        .map(|pred| {
            let mut value =
                serde_json::to_value(pred).expect("predicate can be serialized to json");
            value.sort_all_objects();
            value.to_string()
        })
        .collect();
    set.sort();
    set.dedup();
    // Length prefix so `[P, P]` and `[P]` (equal sets, different lengths) stay distinct.
    format!("{}\u{1e}{}", predicates.len(), set.join("\u{1e}"))
}

/// Analyzes adding a new stub to existing stubs.
///
/// Returns warnings about how the new stub interacts with existing stubs.
pub fn analyze_new_stub(
    existing_stubs: &[Stub],
    new_stub: &Stub,
    insert_index: usize,
) -> StubAnalysisResult {
    let mut result = StubAnalysisResult::new();

    // Check for duplicate ID
    if let Some(new_id) = &new_stub.id {
        for (index, stub) in existing_stubs.iter().enumerate() {
            if stub.id.as_ref() == Some(new_id) {
                result.add_warning(StubWarning {
                    warning_type: WarningType::DuplicateId,
                    message: format!(
                        "New stub has duplicate ID '{new_id}' (same as existing stub at index {index})"
                    ),
                    stub_index: Some(insert_index),
                    stub_id: Some(new_id.clone()),
                    shadowed_by_index: Some(index),
                });
            }
        }
    }

    // Check if new stub is a catch-all
    if new_stub.predicates.is_empty() {
        result.add_warning(StubWarning {
            warning_type: WarningType::CatchAll,
            message: "New stub has empty predicates and will match ALL requests".to_string(),
            stub_index: Some(insert_index),
            stub_id: new_stub.id.clone(),
            shadowed_by_index: None,
        });

        // Warn about stubs it will shadow
        let stubs_after = existing_stubs.len() - insert_index.min(existing_stubs.len());
        if stubs_after > 0 {
            result.add_warning(StubWarning {
                warning_type: WarningType::CatchAllNotLast,
                message: format!(
                    "New catch-all stub will shadow {stubs_after} existing stub(s) after it"
                ),
                stub_index: Some(insert_index),
                stub_id: new_stub.id.clone(),
                shadowed_by_index: None,
            });
        }
    }

    // Check for exact duplicates with existing stubs
    for (index, stub) in existing_stubs.iter().enumerate() {
        if predicates_equal(&new_stub.predicates, &stub.predicates) {
            let (shadower, shadowed) = if index < insert_index {
                (index, insert_index)
            } else {
                (insert_index, index)
            };
            result.add_warning(StubWarning {
                warning_type: WarningType::ExactDuplicate,
                message: format!(
                    "New stub has identical predicates to stub at index {index}. Stub at index {shadower} will shadow the other."
                ),
                stub_index: Some(shadowed),
                stub_id: new_stub.id.clone(),
                shadowed_by_index: Some(shadower),
            });
        }
    }

    // Check if new stub will be shadowed by existing stubs before it
    if !new_stub.predicates.is_empty() {
        for (index, stub) in existing_stubs.iter().enumerate() {
            if index >= insert_index {
                break;
            }
            if stub.predicates.is_empty() {
                result.add_warning(StubWarning {
                    warning_type: WarningType::PotentiallyShadowed,
                    message: format!(
                        "New stub will be shadowed by catch-all stub at index {index}"
                    ),
                    stub_index: Some(insert_index),
                    stub_id: new_stub.id.clone(),
                    shadowed_by_index: Some(index),
                });
            }
        }
    }

    result
}

/// Check if two predicate arrays are exactly equal
fn predicates_equal(a: &[Predicate], b: &[Predicate]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let canonicalize = |pred: &Predicate| {
        let mut value = serde_json::to_value(pred).expect("predicate can be serialized to json");
        value.sort_all_objects();
        value.to_string()
    };
    // Convert to sets for order-independent comparison
    // (predicates are AND'd, so order doesn't matter for matching)
    let a_set: HashSet<String> = a.iter().map(canonicalize).collect();
    let b_set: HashSet<String> = b.iter().map(canonicalize).collect();
    a_set == b_set
}

/// Check if `a` predicates are a subset of `b` predicates.
/// This is a heuristic - if all predicates in `b` are also in `a`,
/// then any request matching `a` would also match `b`.
fn is_subset_predicates(a: &[Predicate], b: &[Predicate]) -> bool {
    if b.is_empty() || a.is_empty() {
        return false;
    }

    // Simple heuristic: check if predicates share the same fields but with different specificity
    // For example, if stub A matches path="/api/users" and stub B matches path="/api",
    // then B is more general and will shadow A for paths starting with /api.

    // Extract field paths from predicates for comparison
    let a_fields = extract_predicate_fields(a);
    let b_fields = extract_predicate_fields(b);

    // If B's fields are a subset of A's fields with the same values, B is more general
    // This is a conservative check - we only flag clear cases
    for (field, b_value) in &b_fields {
        if let Some(a_value) = a_fields.get(field) {
            // Check if B's constraint is more general (e.g., startsWith vs equals)
            if is_more_general_constraint(b_value, a_value) {
                return true;
            }
        }
    }

    false
}

/// Extract field paths from predicates for comparison
fn extract_predicate_fields(predicates: &[Predicate]) -> HashMap<String, PredicateConstraint> {
    let mut fields = HashMap::new();

    for pred in predicates {
        match &pred.operation {
            PredicateOperation::Equals(equals) => {
                fields.extend(
                    equals
                        .iter()
                        .map(|(k, v)| (k.clone(), PredicateConstraint::Equals(v.clone()))),
                );
            }
            PredicateOperation::Contains(contains) => {
                fields.extend(
                    contains
                        .iter()
                        .map(|(k, v)| (k.clone(), PredicateConstraint::Contains(v.clone()))),
                );
            }
            PredicateOperation::StartsWith(starts_with) => {
                fields.extend(
                    starts_with
                        .iter()
                        .map(|(k, v)| (k.clone(), PredicateConstraint::StartsWith(v.clone()))),
                );
            }
            _ => {}
        }
    }

    fields
}

#[derive(Debug, Clone)]
enum PredicateConstraint {
    Equals(serde_json::Value),
    StartsWith(serde_json::Value),
    Contains(serde_json::Value),
}

/// Check if constraint `a` is more general than constraint `b`
fn is_more_general_constraint(a: &PredicateConstraint, b: &PredicateConstraint) -> bool {
    match (a, b) {
        // startsWith is more general than equals if the prefix matches
        (PredicateConstraint::StartsWith(prefix), PredicateConstraint::Equals(exact)) => {
            if let (Some(prefix_str), Some(exact_str)) = (prefix.as_str(), exact.as_str()) {
                exact_str.starts_with(prefix_str)
            } else {
                false
            }
        }
        // contains is more general than equals if the substring is present
        (PredicateConstraint::Contains(needle), PredicateConstraint::Equals(exact)) => {
            if let (Some(needle_str), Some(exact_str)) = (needle.as_str(), exact.as_str()) {
                exact_str.contains(needle_str)
            } else {
                false
            }
        }
        // startsWith is more general than startsWith if it's a prefix of the other
        (PredicateConstraint::StartsWith(a_prefix), PredicateConstraint::StartsWith(b_prefix)) => {
            if let (Some(a_str), Some(b_str)) = (a_prefix.as_str(), b_prefix.as_str()) {
                b_str.starts_with(a_str) && a_str != b_str
            } else {
                false
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn predicates_from_jsons(predicates: Vec<serde_json::Value>) -> Vec<Predicate> {
        predicates
            .into_iter()
            .map(|v| serde_json::from_value(v).unwrap())
            .collect()
    }

    fn stub_with_predicates(predicates: Vec<serde_json::Value>) -> Stub {
        let predicates = predicates_from_jsons(predicates);
        Stub {
            id: None,
            route_pattern: None,
            predicates,
            responses: vec![],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    fn stub_with_id_and_predicates(id: &str, predicates: Vec<serde_json::Value>) -> Stub {
        let predicates = predicates_from_jsons(predicates);
        Stub {
            id: Some(id.to_string()),
            route_pattern: None,
            predicates,
            responses: vec![],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    #[test]
    fn test_duplicate_id_detection() {
        let stubs = vec![
            stub_with_id_and_predicates("stub1", vec![json!({"equals": {"path": "/a"}})]),
            stub_with_id_and_predicates("stub1", vec![json!({"equals": {"path": "/b"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(result.has_warnings());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::DuplicateId)
        );
    }

    #[test]
    fn test_catch_all_detection() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]),
            stub_with_predicates(vec![]), // catch-all
        ];

        let result = analyze_stubs(&stubs);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::CatchAll)
        );
    }

    #[test]
    fn test_catch_all_not_last_warning() {
        let stubs = vec![
            stub_with_predicates(vec![]), // catch-all at start
            stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::CatchAllNotLast)
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::PotentiallyShadowed)
        );
    }

    #[test]
    fn test_exact_duplicate_detection() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"equals": {"path": "/test", "method": "GET"}})]),
            stub_with_predicates(vec![json!({"equals": {"path": "/test", "method": "GET"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate)
        );
    }

    // Issue #423: N identical-predicate stubs must be analyzed in O(N) with a bounded warning set
    // (the old O(N²) exact-duplicate loop emitted ≈N²/2 warnings — hundreds of MB at N=1000).
    #[test]
    fn analyze_stubs_linear_capped_on_overlap() {
        let stubs: Vec<Stub> = (0..500)
            .map(|_| stub_with_predicates(vec![json!({"equals": {"path": "/data"}})]))
            .collect();

        let result = analyze_stubs(&stubs);

        // Bounded: at most the cap plus the single Truncated summary — never O(N²).
        assert!(
            result.warnings.len() <= MAX_STUB_WARNINGS + 1,
            "warnings must be bounded, got {}",
            result.warnings.len()
        );
        // The overlap is still detected...
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate)
        );
        // ...and truncation is reported rather than silently dropped, with the exact count.
        // 500 identical stubs => 499 ExactDuplicate warnings (stubs 1..=499); 100 retained,
        // 399 suppressed (the shadow heuristic is gated off at N=500, so nothing else fires).
        let summary = result
            .warnings
            .iter()
            .find(|w| w.warning_type == WarningType::Truncated)
            .expect("a Truncated summary must record the suppressed warnings");
        assert!(
            summary.message.contains("399 additional"),
            "wrong suppressed count: {}",
            summary.message
        );
    }

    // Issue #423: exact-duplicate detection is now O(n) and points every duplicate at the FIRST
    // occurrence — three identical stubs yield exactly two warnings (not one per earlier pair).
    #[test]
    fn exact_duplicate_points_at_first_occurrence() {
        let stubs: Vec<Stub> = (0..3)
            .map(|_| stub_with_predicates(vec![json!({"equals": {"path": "/same"}})]))
            .collect();

        let result = analyze_stubs(&stubs);
        let dups: Vec<&StubWarning> = result
            .warnings
            .iter()
            .filter(|w| w.warning_type == WarningType::ExactDuplicate)
            .collect();
        assert_eq!(
            dups.len(),
            2,
            "one warning per later duplicate, not per pair"
        );
        assert!(
            dups.iter().all(|w| w.shadowed_by_index == Some(0)),
            "each duplicate must point at the first occurrence"
        );
    }

    // Issue #423: the O(n²) subset-shadowing heuristic is gated off above the threshold, so a
    // general stub followed by many specifics doesn't reintroduce quadratic work — while the same
    // shape below the threshold still produces the advisory warning.
    #[test]
    fn subset_shadow_heuristic_gated_above_threshold() {
        let mut stubs = vec![stub_with_predicates(vec![
            json!({"startsWith": {"path": "/api"}}),
        ])];
        for i in 0..SHADOW_HEURISTIC_MAX_STUBS {
            stubs.push(stub_with_predicates(vec![
                json!({"equals": {"path": format!("/api/{i}")}}),
            ]));
        }
        assert!(stubs.len() > SHADOW_HEURISTIC_MAX_STUBS);
        assert!(
            !analyze_stubs(&stubs)
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::PotentiallyShadowed),
            "subset-shadowing heuristic must be skipped above the threshold"
        );

        let small = vec![
            stub_with_predicates(vec![json!({"startsWith": {"path": "/api"}})]),
            stub_with_predicates(vec![json!({"equals": {"path": "/api/users"}})]),
        ];
        assert!(
            analyze_stubs(&small)
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::PotentiallyShadowed),
            "below the threshold the heuristic still runs"
        );
    }

    #[test]
    fn test_no_warnings_for_different_stubs() {
        let stubs = vec![
            stub_with_id_and_predicates("stub1", vec![json!({"equals": {"path": "/a"}})]),
            stub_with_id_and_predicates("stub2", vec![json!({"equals": {"path": "/b"}})]),
        ];

        let result = analyze_stubs(&stubs);
        // May have warnings about different things, but not duplicates
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::DuplicateId)
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate)
        );
    }

    #[test]
    fn test_shadowing_by_startswith() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"startsWith": {"path": "/api"}})]),
            stub_with_predicates(vec![json!({"equals": {"path": "/api/users"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::PotentiallyShadowed)
        );
    }

    #[test]
    fn test_analyze_new_stub_duplicate_id() {
        let existing = vec![stub_with_id_and_predicates(
            "stub1",
            vec![json!({"equals": {"path": "/a"}})],
        )];
        let new_stub =
            stub_with_id_and_predicates("stub1", vec![json!({"equals": {"path": "/b"}})]);

        let result = analyze_new_stub(&existing, &new_stub, 1);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::DuplicateId)
        );
    }

    #[test]
    fn test_analyze_new_stub_shadowed_by_catchall() {
        let existing = vec![
            stub_with_predicates(vec![]), // catch-all
        ];
        let new_stub = stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]);

        let result = analyze_new_stub(&existing, &new_stub, 1);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::PotentiallyShadowed)
        );
    }

    #[test]
    fn test_predicates_equal_order_independent() {
        let a = vec![
            json!({"equals": {"path": "/test"}}),
            json!({"equals": {"method": "GET"}}),
        ];
        let b = vec![
            json!({"equals": {"method": "GET"}}),
            json!({"equals": {"path": "/test"}}),
        ];
        let a = predicates_from_jsons(a);
        let b = predicates_from_jsons(b);
        assert!(predicates_equal(&a, &b));
    }

    #[test]
    fn test_predicates_not_equal() {
        let a = vec![json!({"equals": {"path": "/test"}})];
        let b = vec![json!({"equals": {"path": "/other"}})];
        let a = predicates_from_jsons(a);
        let b = predicates_from_jsons(b);
        assert!(!predicates_equal(&a, &b));
    }
}
