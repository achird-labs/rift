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

use crate::imposter::Gate;
use crate::imposter::StubResponse;
use crate::imposter::{ImposterConfig, Stub};
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
    /// `_rift.stateOps` (issue #969) declared on a response shape that never runs it — `stateOps`
    /// executes only after an `is` response is rendered (see `extensions::state_ops`'s module
    /// doc). A `proxy`, `inject` or `fault` response does not get this warning: no part of its
    /// `_rift` block applies, and the whole block is reported as `ConfigKeyIgnored` instead. The
    /// reachable case is a `RiftScript` response — which also covers the bare-`_rift` "flat" form
    /// (no `is`/`proxy`/`inject`/`fault`), since that too parses to `RiftScript`.
    StateOpsNeverRuns,
    /// `_rift.conditional` (issue #1280) that can never answer 304 (issue #1296): it is read only
    /// on an `is` response, and only for GET/HEAD. Raised for a script-only response, and for an
    /// `is` response in a stub whose top-level `method` predicate excludes GET and HEAD. A
    /// `proxy`/`inject`/`fault` response gets [`WarningType::ConfigKeyIgnored`] instead.
    ConditionalNeverRuns,
    /// A key this engine parses and does not act on (issue #1152). The value reads back unchanged,
    /// so without this nothing distinguishes "honoured" from "dropped". See
    /// [`ignored_config_keys`] for the list.
    ConfigKeyIgnored,
}

/// The shape of a response that carries a `_rift` block no feature applies to.
fn ignored_rift_shape(response: &StubResponse) -> Option<&'static str> {
    match response {
        StubResponse::Proxy {
            ignored_rift: Some(_),
            ..
        } => Some("proxy"),
        StubResponse::Inject {
            ignored_rift: Some(_),
            ..
        } => Some("inject"),
        StubResponse::Fault {
            ignored_rift: Some(_),
            ..
        } => Some("fault"),
        _ => None,
    }
}

/// The shape of a response whose behaviors block holds something no behavior runs on (issue #1181).
/// Behaviors run on `is`, `inject` and `proxy` responses; `repeat` applies to every response (#1188), so a
/// block that sets nothing else is not ignored.
fn ignored_behaviors_shape(response: &StubResponse) -> Option<&'static str> {
    let (shape, block) = match response {
        StubResponse::Fault {
            ignored_behaviors: Some(block),
            ..
        } => ("fault", block),
        StubResponse::RiftScript {
            ignored_behaviors: Some(block),
            ..
        } => ("_rift", block),
        _ => return None,
    };
    // The block is a compiled program (issue #1198): one element per step, `null`s already applied.
    let sets_more_than_repeat = block.as_array().is_some_and(|program| {
        program
            .iter()
            .filter_map(serde_json::Value::as_object)
            .any(|element| element.keys().any(|k| k != "repeat"))
    });
    sets_more_than_repeat.then_some(shape)
}

/// The stubs with a response `shape_of` classifies as `shape`: the first index, and the indices as
/// a warning lists them — at most ten, then "and N more".
fn stubs_with_shape(
    stubs: &[Stub],
    shape: &str,
    shape_of: fn(&StubResponse) -> Option<&'static str>,
) -> Option<(usize, String)> {
    const LISTED: usize = 10;
    let indices: Vec<usize> = stubs
        .iter()
        .enumerate()
        .filter(|(_, stub)| stub.responses.iter().any(|r| shape_of(r) == Some(shape)))
        .map(|(index, _)| index)
        .collect();
    let &first = indices.first()?;
    let mut listed = indices
        .iter()
        .take(LISTED)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if indices.len() > LISTED {
        listed.push_str(&format!(" and {} more", indices.len() - LISTED));
    }
    Some((first, listed))
}

/// Every key in `config` that this engine parses and does not act on — the single list behind the
/// load-time log line, the `_rift.warnings` entries and the docs (issue #1152). `stubs` are the
/// imposter's current stubs, which a stub mutation may have changed since `config` was built.
///
/// The keys are kept, not refused: the SDKs emit several of them (`metrics`, `proxyPool`,
/// `recordMatches`), and a refusal would break every SDK user who touched those builders.
pub fn ignored_config_keys(config: &ImposterConfig, stubs: &[Stub]) -> Vec<StubWarning> {
    let imposter_level = |message: &str| StubWarning {
        warning_type: WarningType::ConfigKeyIgnored,
        message: message.to_owned(),
        stub_index: None,
        stub_id: None,
        shadowed_by_index: None,
    };
    let mut warnings = Vec::new();
    let rift = config.rift.as_ref();
    if rift.is_some_and(|r| r.metrics.is_some()) {
        warnings.push(imposter_level(
            "`_rift.metrics` has no effect: metrics are process-wide, served on --metrics-port \
             (default 9090), and not configurable per imposter",
        ));
    }
    if rift.is_some_and(|r| r.proxy.is_some()) {
        warnings.push(imposter_level(
            "`_rift.proxy` has no effect: a proxy response's upstream is its own `proxy.to`, and \
             connection pooling is not configurable per imposter",
        ));
    }
    if config.record_matches {
        warnings.push(imposter_level(
            "`recordMatches` has no effect: this engine does not record per-stub `matches`; use \
             `recordRequests` and GET /imposters/:port to see the requests",
        ));
    }
    // One entry per shape, however many stubs carry it: a generated imposter can put `_rift` on
    // every response, and one entry per response would undo the MAX_STUB_WARNINGS bound (#423).
    let per_shape = |first: usize, message: String| StubWarning {
        warning_type: WarningType::ConfigKeyIgnored,
        message,
        stub_index: Some(first),
        stub_id: stubs[first].id.clone(),
        shadowed_by_index: None,
    };
    for shape in ["proxy", "inject", "fault"] {
        if let Some((first, listed)) = stubs_with_shape(stubs, shape, ignored_rift_shape) {
            warnings.push(per_shape(
                first,
                format!(
                    "`_rift` on a `{shape}` response has no effect: no `_rift` feature applies to \
                     a `{shape}` response (stubs {listed})"
                ),
            ));
        }
    }
    for (shape, noun, why) in [
        (
            "fault",
            "a `fault` response",
            "the rest do not apply to a fault, as in Mountebank",
        ),
        (
            "_rift",
            "a `_rift`-only response",
            "the rest do not apply to a script response",
        ),
    ] {
        if let Some((first, listed)) = stubs_with_shape(stubs, shape, ignored_behaviors_shape) {
            warnings.push(per_shape(
                first,
                format!(
                    "A behaviors block on {noun} has no effect except `repeat`: {why} \
                     (stubs {listed})"
                ),
            ));
        }
    }
    warnings
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
    // Each key holds every `(gate, index)` first seen with it: a stub is a duplicate only of an
    // earlier stub whose gate covers its own (issue #1308), so the bucket is scanned for one.
    let mut seen_predicates: HashMap<String, Vec<(Gate<'_>, usize)>> = HashMap::new();
    // Index of the first catch-all (empty-predicate) stub seen so far, of any gate. Drives the
    // `CatchAll` / `CatchAllNotLast` summaries.
    let mut first_catch_all: Option<usize> = None;
    // The catch-alls that shadow later stubs (issue #1308): the first ungated one covers every
    // gate; a gated one covers only stubs behind its exact gate.
    let mut first_ungated_catch_all: Option<usize> = None;
    let mut first_catch_all_by_gate: HashMap<Gate<'_>, usize> = HashMap::new();
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
            let gate = stub.gate();
            if gate.is_open() {
                first_ungated_catch_all.get_or_insert(index);
            } else {
                first_catch_all_by_gate.entry(gate).or_insert(index);
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

        // `_rift.stateOps` on a response shape that never runs it (issue #969). `proxy`/`inject`
        // keep their `_rift` as `ignored_rift` (reported as `ConfigKeyIgnored`), so the only
        // reachable shape here is `RiftScript` (which also covers the bare-`_rift` "flat" form —
        // see `WarningType::StateOpsNeverRuns`).
        for response in &stub.responses {
            if let StubResponse::RiftScript { rift, .. } = response
                && !rift.state_ops.is_empty()
            {
                push(
                    &mut result.warnings,
                    StubWarning {
                        warning_type: WarningType::StateOpsNeverRuns,
                        message: format!(
                            "Stub at index {index} has _rift.stateOps on a non-`is` response; \
                             stateOps only runs after an `is` response is rendered, so these \
                             operations never execute"
                        ),
                        stub_index: Some(index),
                        stub_id: stub.id.clone(),
                        shadowed_by_index: None,
                    },
                );
            }
        }

        // `_rift.conditional` that can never answer 304 (issue #1296).
        let excluded_method = non_get_head_method(&stub.predicates);
        for response in &stub.responses {
            let message = match response {
                StubResponse::RiftScript { rift, .. } if declares_conditional(rift) => format!(
                    "Stub at index {index} has _rift.conditional on a script-only response; \
                     conditional GET applies only to an `is` response (move it to an `is`, or \
                     drop it)"
                ),
                StubResponse::Is {
                    rift: Some(rift), ..
                } if declares_conditional(rift) => {
                    let Some(method) = excluded_method else {
                        continue;
                    };
                    format!(
                        "Stub at index {index} has _rift.conditional but its method predicate \
                         ({method}) excludes GET and HEAD, so it never answers 304"
                    )
                }
                _ => continue,
            };
            push(
                &mut result.warnings,
                StubWarning {
                    warning_type: WarningType::ConditionalNeverRuns,
                    message,
                    stub_index: Some(index),
                    stub_id: stub.id.clone(),
                    shadowed_by_index: None,
                },
            );
        }

        // Exact predicate duplicates — O(1) hash lookup against the first stub with this key.
        let key = predicate_key(&stub.predicates);
        let gate = stub.gate();
        let bucket = seen_predicates.entry(key).or_default();
        match bucket.iter().find(|(earlier, _)| earlier.covers(&gate)) {
            Some(&(_, first_index)) => push(
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
            None => bucket.push((gate, index)),
        }

        // Potential shadowing of a specific (non-empty) stub by an earlier one.
        if !stub.predicates.is_empty() {
            // An earlier catch-all shadows this stub when its gate covers the stub's: the first
            // ungated one always does, else the one behind the stub's exact gate — O(1).
            let catch_all = first_ungated_catch_all
                .or_else(|| first_catch_all_by_gate.get(&stub.gate()).copied());
            if let Some(catch_all_index) = catch_all {
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
                        && earlier_stub.gate().covers(&stub.gate())
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
/// `_rift.conditional` is declared and not switched off with `false`.
fn declares_conditional(rift: &crate::imposter::RiftResponseExtension) -> bool {
    !matches!(
        rift.conditional,
        None | Some(crate::imposter::ConditionalGet::Enabled(false))
    )
}

/// The value of a top-level `equals`/`deepEquals` `method` predicate that is a single string other
/// than GET/HEAD (compared case-insensitively), so conditional GET can never apply behind it.
///
/// Deliberately narrow: a `method` under `or`/`not`/`and`, a `matches`/`exists`, a non-string value
/// or a predicate with `except`/a selector is not judged. A false negative is fine; a false
/// positive on a working stub is not.
fn non_get_head_method(predicates: &[Predicate]) -> Option<&str> {
    predicates.iter().find_map(|p| {
        let (PredicateOperation::Equals(fields) | PredicateOperation::DeepEquals(fields)) =
            &p.operation
        else {
            return None;
        };
        if !p.parameters.except.is_empty() || p.parameters.selector.is_some() {
            return None;
        }
        let method = fields.get("method")?.as_str()?;
        (!method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD"))
            .then_some(method)
    })
}

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
            // Only the earlier of the two can kill the later, and only when its gate covers it.
            let (earlier_gate, later_gate) = if index < insert_index {
                (stub.gate(), new_stub.gate())
            } else {
                (new_stub.gate(), stub.gate())
            };
            if !earlier_gate.covers(&later_gate) {
                continue;
            }
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
            if stub.predicates.is_empty() && stub.gate().covers(&new_stub.gate()) {
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
    use crate::imposter::RiftResponseExtension;
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

    // Issue #969: `_rift.stateOps` on a `RiftScript` response (the reachable non-`is` shape —
    // `proxy`/`inject` drop `_rift` entirely at parse time) must warn, since stateOps never runs
    // outside an `is` response.
    #[test]
    fn state_ops_on_a_rift_script_response_warns() {
        let rift: RiftResponseExtension = serde_json::from_value(json!({
            "stateOps": [{ "op": "increment", "key": "hits" }]
        }))
        .expect("parses");
        let stub = Stub {
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::RiftScript {
                rift: Box::new(rift),
                ignored_behaviors: None,
            }],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        };

        let result = analyze_stubs(&[stub]);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::StateOpsNeverRuns),
            "a RiftScript response's stateOps must be flagged as never running: {:?}",
            result.warnings
        );
    }

    // A `RiftScript` response with NO `stateOps` (an actual script-only response) must not warn —
    // only a non-empty `stateOps` block on a non-`is` response is the problem.
    #[test]
    fn a_script_only_response_without_state_ops_does_not_warn() {
        let rift: RiftResponseExtension = serde_json::from_value(json!({
            "script": { "code": "response.body = 'x';" }
        }))
        .expect("parses");
        let stub = Stub {
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::RiftScript {
                rift: Box::new(rift),
                ignored_behaviors: None,
            }],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        };

        let result = analyze_stubs(&[stub]);
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::StateOpsNeverRuns),
            "a script-only response with no stateOps must not warn: {:?}",
            result.warnings
        );
    }

    // `stateOps` on an `is` response — the reachable, correct shape — must never warn.
    #[test]
    fn state_ops_on_an_is_response_does_not_warn() {
        let rift: RiftResponseExtension = serde_json::from_value(json!({
            "stateOps": [{ "op": "increment", "key": "hits" }]
        }))
        .expect("parses");
        let is_response = crate::imposter::StubResponse::new_is(
            crate::imposter::IsResponse {
                status_code: 200,
                headers: Default::default(),
                body: None,
                mode: Default::default(),
            },
            None,
            Some(Box::new(rift)),
        );
        let stub = Stub {
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![is_response],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        };

        let result = analyze_stubs(&[stub]);
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.warning_type == WarningType::StateOpsNeverRuns),
            "stateOps on an `is` response must not warn: {:?}",
            result.warnings
        );
    }

    // ─── Issue #1296: `_rift.conditional` that can never fire ───────────────────────────────

    fn conditional_stub(predicates: serde_json::Value, script_only: bool) -> Stub {
        let rift: RiftResponseExtension = serde_json::from_value(json!({
            "conditional": true,
            "script": { "code": "response.body = 'x';" }
        }))
        .expect("parses");
        let response = if script_only {
            StubResponse::RiftScript {
                rift: Box::new(rift),
                ignored_behaviors: None,
            }
        } else {
            crate::imposter::StubResponse::new_is(
                crate::imposter::IsResponse {
                    status_code: 200,
                    headers: Default::default(),
                    body: None,
                    mode: Default::default(),
                },
                None,
                Some(Box::new(rift)),
            )
        };
        Stub {
            id: None,
            route_pattern: None,
            predicates: serde_json::from_value(predicates).expect("predicates parse"),
            responses: vec![response],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    fn conditional_warnings(stub: Stub) -> Vec<StubWarning> {
        analyze_stubs(&[stub])
            .warnings
            .into_iter()
            .filter(|w| w.warning_type == WarningType::ConditionalNeverRuns)
            .collect()
    }

    #[test]
    fn conditional_on_script_only_warns() {
        let w = conditional_warnings(conditional_stub(json!([]), true));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].message.contains("script-only"), "{}", w[0].message);
    }

    #[test]
    fn conditional_behind_a_post_method_predicate_warns() {
        let w = conditional_warnings(conditional_stub(
            json!([{ "equals": { "method": "post" } }]),
            false,
        ));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].message.contains("post"), "{}", w[0].message);
        let w = conditional_warnings(conditional_stub(
            json!([{ "deepEquals": { "method": "POST" } }]),
            false,
        ));
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn conditional_on_a_get_stub_is_quiet() {
        for preds in [
            json!([]),
            json!([{ "equals": { "method": "GET" } }]),
            json!([{ "equals": { "method": "head" } }]),
            json!([{ "equals": { "path": "/x" } }]),
        ] {
            let w = conditional_warnings(conditional_stub(preds.clone(), false));
            assert!(w.is_empty(), "{preds}: {w:?}");
        }
    }

    #[test]
    fn conditional_under_or_is_not_judged() {
        for preds in [
            json!([{ "or": [{ "equals": { "method": "POST" } }, { "equals": { "method": "GET" } }] }]),
            json!([{ "not": { "equals": { "method": "GET" } } }]),
            json!([{ "matches": { "method": "^P" } }]),
            json!([{ "exists": { "method": true } }]),
            json!([{ "equals": { "method": 5 } }]),
        ] {
            let w = conditional_warnings(conditional_stub(preds.clone(), false));
            assert!(w.is_empty(), "{preds}: {w:?}");
        }
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

    // Issue #1308: the analysis must read the same gates the matcher does.
    fn gated_stub(
        space: Option<&str>,
        scenario: Option<(&str, &str)>,
        predicates: Vec<serde_json::Value>,
    ) -> Stub {
        let mut stub = stub_with_predicates(predicates);
        stub.space = space.map(str::to_string);
        if let Some((name, state)) = scenario {
            stub.scenario_name = Some(name.to_string());
            stub.required_scenario_state = Some(state.to_string());
        }
        stub
    }

    fn count(result: &StubAnalysisResult, ty: WarningType) -> usize {
        result
            .warnings
            .iter()
            .filter(|w| w.warning_type == ty)
            .count()
    }

    fn pay() -> Vec<serde_json::Value> {
        vec![json!({"equals": {"method": "POST", "path": "/pay"}})]
    }

    #[test]
    fn a_scenario_gated_pair_with_identical_predicates_is_not_a_duplicate() {
        let stubs = vec![
            gated_stub(None, Some(("checkout", "Started")), pay()),
            gated_stub(None, Some(("checkout", "paid")), pay()),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::ExactDuplicate), 0);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 0);
    }

    #[test]
    fn stubs_in_different_spaces_are_neither_duplicates_nor_shadowing() {
        let stubs = vec![
            gated_stub(Some("a"), None, vec![]),
            gated_stub(Some("b"), None, vec![]),
            gated_stub(Some("a"), None, pay()),
            gated_stub(Some("b"), None, pay()),
            gated_stub(
                Some("c"),
                None,
                vec![json!({"startsWith": {"path": "/api"}})],
            ),
            gated_stub(
                Some("d"),
                None,
                vec![json!({"equals": {"path": "/api/users"}})],
            ),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::ExactDuplicate), 0);
        // stubs 2 and 3 are shadowed by their own space's catch-all, and nothing else.
        let shadowed: Vec<_> = result
            .warnings
            .iter()
            .filter(|w| w.warning_type == WarningType::PotentiallyShadowed)
            .map(|w| (w.stub_index, w.shadowed_by_index))
            .collect();
        assert_eq!(shadowed, vec![(Some(2), Some(0)), (Some(3), Some(1))]);
    }

    #[test]
    fn a_gated_catch_all_does_not_shadow_an_ungated_stub() {
        let stubs = vec![
            gated_stub(None, Some(("s", "Started")), vec![]),
            gated_stub(None, None, vec![json!({"equals": {"path": "/x"}})]),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 0);
    }

    #[test]
    fn the_subset_heuristic_honours_gates() {
        let stubs = vec![
            gated_stub(
                None,
                Some(("s", "Started")),
                vec![json!({"startsWith": {"path": "/api"}})],
            ),
            gated_stub(
                None,
                Some(("s", "paid")),
                vec![json!({"equals": {"path": "/api/users"}})],
            ),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 0);
    }

    #[test]
    fn analyze_new_stub_honours_gates_for_duplicates_and_catch_alls() {
        let started = gated_stub(None, Some(("s", "Started")), pay());
        let paid = gated_stub(None, Some(("s", "paid")), pay());
        let result = analyze_new_stub(std::slice::from_ref(&started), &paid, 1);
        assert_eq!(count(&result, WarningType::ExactDuplicate), 0);

        let gated_catch_all = gated_stub(None, Some(("s", "Started")), vec![]);
        let ungated = stub_with_predicates(pay());
        let result = analyze_new_stub(std::slice::from_ref(&gated_catch_all), &ungated, 1);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 0);

        let same_gate = gated_stub(None, Some(("s", "Started")), pay());
        let result = analyze_new_stub(std::slice::from_ref(&gated_catch_all), &same_gate, 1);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 1);
    }

    #[test]
    fn an_ungated_twin_ahead_still_makes_a_gated_stub_a_duplicate() {
        let stubs = vec![
            stub_with_predicates(pay()),
            gated_stub(None, Some(("s", "paid")), pay()),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::ExactDuplicate), 1);
        let result = analyze_new_stub(&stubs[..1], &stubs[1], 1);
        assert_eq!(count(&result, WarningType::ExactDuplicate), 1);
    }

    #[test]
    fn an_ungated_catch_all_still_shadows_a_gated_stub() {
        let stubs = vec![
            stub_with_predicates(vec![]),
            gated_stub(None, Some(("s", "paid")), pay()),
        ];
        let result = analyze_stubs(&stubs);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 1);
        let result = analyze_new_stub(&stubs[..1], &stubs[1], 1);
        assert_eq!(count(&result, WarningType::PotentiallyShadowed), 1);
    }
}
