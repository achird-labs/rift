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

    // Track IDs for duplicate detection
    let mut seen_ids: HashMap<String, usize> = HashMap::new();

    // Track catch-all stubs
    let mut catch_all_indices: Vec<usize> = vec![];

    for (index, stub) in stubs.iter().enumerate() {
        // Check for duplicate IDs
        if let Some(id) = &stub.id {
            if let Some(&existing_index) = seen_ids.get(id) {
                result.add_warning(StubWarning {
                    warning_type: WarningType::DuplicateId,
                    message: format!(
                        "Stub at index {index} has duplicate ID '{id}' (same as stub at index {existing_index})"
                    ),
                    stub_index: Some(index),
                    stub_id: Some(id.clone()),
                    shadowed_by_index: Some(existing_index),
                });
            } else {
                seen_ids.insert(id.clone(), index);
            }
        }

        // Check for catch-all stubs (empty predicates)
        if stub.predicates.is_empty() {
            catch_all_indices.push(index);
            result.add_warning(StubWarning {
                warning_type: WarningType::CatchAll,
                message: format!(
                    "Stub at index {index} has empty predicates and will match ALL requests"
                ),
                stub_index: Some(index),
                stub_id: stub.id.clone(),
                shadowed_by_index: None,
            });
        }

        // Check for exact predicate duplicates
        for (earlier_index, earlier_stub) in stubs[..index].iter().enumerate() {
            if predicates_equal(&stub.predicates, &earlier_stub.predicates) {
                result.add_warning(StubWarning {
                    warning_type: WarningType::ExactDuplicate,
                    message: format!(
                        "Stub at index {index} has identical predicates to stub at index {earlier_index} and will never match"
                    ),
                    stub_index: Some(index),
                    stub_id: stub.id.clone(),
                    shadowed_by_index: Some(earlier_index),
                });
            }
        }

        // Check for potential shadowing by earlier stubs
        // This is a heuristic check for common shadowing scenarios
        if !stub.predicates.is_empty() {
            for (earlier_index, earlier_stub) in stubs[..index].iter().enumerate() {
                if earlier_stub.predicates.is_empty() {
                    // Catch-all before this stub
                    result.add_warning(StubWarning {
                        warning_type: WarningType::PotentiallyShadowed,
                        message: format!(
                            "Stub at index {index} may be shadowed by catch-all stub at index {earlier_index}"
                        ),
                        stub_index: Some(index),
                        stub_id: stub.id.clone(),
                        shadowed_by_index: Some(earlier_index),
                    });
                } else if is_subset_predicates(&stub.predicates, &earlier_stub.predicates) {
                    // Earlier stub has more specific predicates that are a subset
                    // This means the earlier stub will match first for overlapping requests
                    result.add_warning(StubWarning {
                        warning_type: WarningType::PotentiallyShadowed,
                        message: format!(
                            "Stub at index {index} may be partially shadowed by stub at index {earlier_index} which has overlapping predicates"
                        ),
                        stub_index: Some(index),
                        stub_id: stub.id.clone(),
                        shadowed_by_index: Some(earlier_index),
                    });
                }
            }
        }
    }

    // Warn if catch-all is not at the end
    if let Some(&catch_all_idx) = catch_all_indices.first() {
        if catch_all_idx < stubs.len() - 1 {
            result.add_warning(StubWarning {
                warning_type: WarningType::CatchAllNotLast,
                message: format!(
                    "Catch-all stub at index {} will shadow {} stub(s) after it",
                    catch_all_idx,
                    stubs.len() - catch_all_idx - 1
                ),
                stub_index: Some(catch_all_idx),
                stub_id: stubs[catch_all_idx].id.clone(),
                shadowed_by_index: None,
            });
        }
    }

    result
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
            predicates,
            responses: vec![],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
        }
    }

    fn stub_with_id_and_predicates(id: &str, predicates: Vec<serde_json::Value>) -> Stub {
        let predicates = predicates_from_jsons(predicates);
        Stub {
            id: Some(id.to_string()),
            predicates,
            responses: vec![],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
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
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::DuplicateId));
    }

    #[test]
    fn test_catch_all_detection() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]),
            stub_with_predicates(vec![]), // catch-all
        ];

        let result = analyze_stubs(&stubs);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::CatchAll));
    }

    #[test]
    fn test_catch_all_not_last_warning() {
        let stubs = vec![
            stub_with_predicates(vec![]), // catch-all at start
            stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::CatchAllNotLast));
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::PotentiallyShadowed));
    }

    #[test]
    fn test_exact_duplicate_detection() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"equals": {"path": "/test", "method": "GET"}})]),
            stub_with_predicates(vec![json!({"equals": {"path": "/test", "method": "GET"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::ExactDuplicate));
    }

    #[test]
    fn test_no_warnings_for_different_stubs() {
        let stubs = vec![
            stub_with_id_and_predicates("stub1", vec![json!({"equals": {"path": "/a"}})]),
            stub_with_id_and_predicates("stub2", vec![json!({"equals": {"path": "/b"}})]),
        ];

        let result = analyze_stubs(&stubs);
        // May have warnings about different things, but not duplicates
        assert!(!result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::DuplicateId));
        assert!(!result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::ExactDuplicate));
    }

    #[test]
    fn test_shadowing_by_startswith() {
        let stubs = vec![
            stub_with_predicates(vec![json!({"startsWith": {"path": "/api"}})]),
            stub_with_predicates(vec![json!({"equals": {"path": "/api/users"}})]),
        ];

        let result = analyze_stubs(&stubs);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::PotentiallyShadowed));
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
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::DuplicateId));
    }

    #[test]
    fn test_analyze_new_stub_shadowed_by_catchall() {
        let existing = vec![
            stub_with_predicates(vec![]), // catch-all
        ];
        let new_stub = stub_with_predicates(vec![json!({"equals": {"path": "/specific"}})]);

        let result = analyze_new_stub(&existing, &new_stub, 1);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.warning_type == WarningType::PotentiallyShadowed));
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
