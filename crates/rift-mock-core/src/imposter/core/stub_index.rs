//! Stage-1 candidate prefilter for imposter stub matching (issues #292, #707).
//!
//! The imposter match loop (`core/matching.rs`) would otherwise be a linear scan running full
//! Mountebank predicate evaluation on every stub. This index narrows that to a candidate set, so
//! only stubs that *could* match are evaluated.
//!
//! # The dimension framework (issue #707)
//!
//! The index is a set of independent **dimensions**, one per request attribute (path, method, path
//! literals (#710), path regexes (#709), and — via the sibling issue built on this seam — deepEquals
//! bodies (#708)). Each dimension answers one question for a request: *which stubs can this
//! attribute not rule out?* The answers are [`CandidateBits`] — dense bitsets over stub ids, where
//! a stub's id is its position in the snapshot's stub vector (declaration-ordered).
//!
//! [`StubIndex::candidates`] ANDs the per-dimension bitsets and walks the surviving bits ascending.
//! This is the Lucent Bit Vector technique from packet classification: each dimension prunes
//! independently, and the intersection is the candidate set. Ascending iteration *is* Mountebank's
//! first-match-wins order, so Stage-2 evaluation order is unchanged.
//!
//! ## The soundness rule — the one invariant every dimension must uphold
//!
//! A dimension's bitset for a request is `matched_bits | always_bits`, where:
//!
//! * `matched_bits` — stubs whose constraint on this attribute the request *satisfies*;
//! * `always_bits` — stubs that either do not constrain this attribute at all, or constrain it in
//!   a shape this dimension cannot index (precomputed once at build).
//!
//! **A dimension may only ever exclude a stub it can prove cannot match.** Everything else must be
//! in `always_bits`. The index is therefore a strict *over-approximation*: `candidates()` returns a
//! superset of the true matches, and full predicate verification (`stub_matches`, unchanged) stays
//! the single source of truth for semantics — including `and`/`or`/`not`, `except`, selectors,
//! case flags, and `inject`. Widening a dimension's eligibility later is a pure optimization, never
//! a semantics question.
//!
//! Eligibility is deliberately conservative and uniform across dimensions: a stub is indexed only
//! when a *top-level* (implicitly AND-ed) predicate constrains the raw field — no `selector`, no
//! `except` ([`is_value_preserving`]). Such a predicate is *required* for the stub to match, so a
//! request failing it can never match the stub → safe to exclude. The dimensions that compare
//! strings themselves additionally require not-`caseSensitive` ([`is_safely_indexable`]), because
//! they fold both sides eagerly at build; the regex dimension instead routes the case flag to one
//! of two automata, which is why the two gates are separate.
//!
//! ## Adding a dimension
//!
//! Implement [`Dimension`], add it as a field of [`StubIndex`], build its `always_bits` from the
//! stubs it cannot index, and AND it in [`StubIndex::candidates`]. Dimensions are concrete fields
//! rather than `Box<dyn Dimension>` so the match loop dispatches statically and allocates nothing
//! extra. The guardrail is `differential_index_matches_linear_oracle` below: a dimension that
//! under-approximates fails it immediately.

use super::StubState;
use super::bitset::CandidateBits;
use crate::imposter::predicates::json::structural_hash;
use crate::imposter::types::Stub;
use crate::util::FastMap;
use aho_corasick::{AhoCorasick, MatchKind as AcMatchKind};
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, PatternSet, meta};
use rift_types::predicate::{Predicate, PredicateOperation};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// The request attributes the index prunes on. Extended as sibling dimensions land.
pub(crate) struct DimensionRequest<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    /// The request body parsed as JSON once per request (#290), if it parsed. `None` for a non-JSON
    /// or absent body — the body dimension (#708) can only prune JSON request bodies.
    pub(crate) body: Option<&'a serde_json::Value>,
}

/// One pruning dimension of the index. See the module docs for the soundness rule this contract
/// requires: `select` must set a bit for **every** stub the request cannot rule out.
trait Dimension {
    /// Write `matched_bits | always_bits` for `request` into `out`, overwriting it entirely.
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits);

    /// Whether any stub is indexed on this dimension at all.
    ///
    /// When none is, `always_bits` is all-ones, so `select` can only ever produce all-ones and
    /// intersecting it is a no-op — [`StubIndex::candidates`] skips the dimension entirely rather
    /// than pay a full-width copy and intersect to learn nothing. Constant per snapshot.
    fn prunes(&self) -> bool;
}

/// A required path constraint extracted from a stub's top-level predicates.
///
/// `startsWith`/`contains`/`endsWith` moved off this enum in #710 — they are now the literal
/// dimension's `LiteralAnchor`, indexed by Aho-Corasick instead of a per-anchor bucket walk. Only
/// `equals` remains here.
enum PathAnchor {
    Exact(String),
}

/// The `path` value of a predicate's field map, folded for indexing, if present and a string.
fn field_path(fields: &HashMap<String, serde_json::Value>) -> Option<String> {
    match fields.get("path") {
        Some(serde_json::Value::String(s)) => Some(fold(s)),
        _ => None,
    }
}

/// The case fold the index compares under.
///
/// This MUST be the evaluator's fold, not merely *a* fold. The default (non-`caseSensitive`)
/// comparison in `predicates::mod` is `eq_ignore_ascii_case` / `starts_with_ignore_ascii_case` /
/// `contains_ignore_ascii_case` — **ASCII**. Folding both sides with `to_ascii_lowercase` is
/// exactly equivalent to those, so the path dimension neither over- nor under-approximates.
///
/// Unicode `to_lowercase` would be wrong here, and not merely conservative: it is length-changing
/// and context-sensitive, so it breaks the prefix/substring relation the dimension relies on. Stub
/// `startsWith "/ΟΣ"` vs request `/ΟΣΑ` is the counter-example — the evaluator matches (its ASCII
/// fold leaves Greek untouched), but Unicode-lowercasing the anchor yields a final sigma (`/ος`)
/// that `"/οσα"` does not start with, so the stub would be pruned and silently stop matching.
fn fold(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Whether a predicate compares its field values against the **raw** request value — the soundness
/// gate every dimension's eligibility rule shares.
///
/// Anything that transforms or re-scopes the compared value cannot be indexed against the raw
/// field: `except` rewrites the value before comparison and `selector` re-scopes it. One home for
/// the rule, because the dimensions added on this seam (#708-#710) must not let their copies of it
/// drift.
fn is_value_preserving(p: &rift_types::predicate::PredicateParameters) -> bool {
    p.except.is_empty() && p.selector.is_none()
}

/// [`is_value_preserving`] plus the fold requirement, for the dimensions that compare *strings*
/// themselves.
///
/// `caseSensitive` opts out of the fold [`fold`] assumes, and the path/method dimensions have no
/// way to represent a case-sensitive compare — they fold both sides eagerly at build. The regex
/// dimension does not share that limitation (the automaton carries its own case flag), so it gates
/// on [`is_value_preserving`] and routes on the flag instead.
fn is_safely_indexable(p: &rift_types::predicate::PredicateParameters) -> bool {
    is_value_preserving(p) && p.case_sensitive != Some(true)
}

/// A single predicate's path anchor, if it is a safely-indexable required `equals` path
/// constraint. `startsWith`/`contains`/`endsWith` are the literal dimension's concern (#710).
fn path_anchor(pred: &Predicate) -> Option<PathAnchor> {
    if !is_safely_indexable(&pred.parameters) {
        return None;
    }
    match &pred.operation {
        PredicateOperation::Equals(fields) => field_path(fields).map(PathAnchor::Exact),
        _ => None,
    }
}

/// The first required path anchor among a stub's top-level (AND-ed) predicates, or `None` if the
/// stub can't be safely path-indexed (→ `always_bits`).
fn classify(stub: &Stub) -> Option<PathAnchor> {
    stub.predicates.iter().find_map(path_anchor)
}

/// The exact-path dimension (issue #292, ported onto the #707 bitset framework).
///
/// Handles only `equals` on `path` — an O(1) hashmap lookup per request. `startsWith`/`contains`/
/// `endsWith` used to be linear-walked buckets on this same type; #710 supersedes both walks with
/// the literal dimension's Aho-Corasick pass, so this type shrinks to just the exact bucket. A
/// stub can be indexed on both: an `equals` anchor here and a `startsWith` anchor on the literal
/// dimension are independent required constraints, and both are allowed to prune.
///
/// The bucket stays `Vec<usize>` rather than a bitset: a bucket holds only the stubs sharing an
/// anchor, so materializing it costs O(matched) rather than O(stubs/64), and build memory stays
/// O(stubs) instead of O(stubs x buckets).
struct PathDimension {
    // Rebuilt on every stub-set replace/mutation (issue #704); its keys come from operator stub
    // config, not request traffic — see `crate::util::fastmap` doc for the HashDoS policy.
    exact: FastMap<String, Vec<usize>>,
    /// Stubs with no indexable top-level `equals` path constraint — always candidates on this
    /// dimension.
    always: CandidateBits,
}

impl Dimension for PathDimension {
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits) {
        out.copy_from(&self.always);
        // Anchors were folded at build; fold the request the same way — see `fold`. Request paths
        // are overwhelmingly already lowercase, so probe the map with the borrowed `&str` directly
        // and pay for the folded `String` only when an uppercase ASCII byte is actually present
        // (issue #731 — the fold's only effect is ASCII case, so a path with no uppercase byte
        // folds to itself).
        let matched = if request.path.bytes().any(|b| b.is_ascii_uppercase()) {
            self.exact.get(&fold(request.path))
        } else {
            self.exact.get(request.path)
        };
        if let Some(v) = matched {
            v.iter().for_each(|i| out.set(*i));
        }
    }

    fn prunes(&self) -> bool {
        !self.exact.is_empty()
    }
}

/// The fixed method slots. Any method outside the standard set (or a stub constraining an
/// unusual one) shares `Other` — a coarser bucket is still sound, it just prunes less.
const METHOD_SLOTS: usize = 8;
const SLOT_OTHER: usize = METHOD_SLOTS - 1;

/// The slot a method name belongs to, matched case-insensitively and without allocating.
fn method_slot(method: &str) -> usize {
    const NAMED: [&str; SLOT_OTHER] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
    NAMED
        .iter()
        .position(|m| method.eq_ignore_ascii_case(m))
        .unwrap_or(SLOT_OTHER)
}

/// The method required by a single predicate, if it is a safely-indexable required constraint.
fn method_anchor(pred: &Predicate) -> Option<&str> {
    if !is_safely_indexable(&pred.parameters) {
        return None;
    }
    match &pred.operation {
        PredicateOperation::Equals(fields) => match fields.get("method") {
            Some(serde_json::Value::String(s)) => Some(s.as_str()),
            _ => None,
        },
        _ => None,
    }
}

/// The method dimension (issue #707) — the cheapest possible dimension, and the proof of the
/// framework: a POST-only stub stops being a candidate for every GET.
struct MethodDimension {
    slots: [Vec<usize>; METHOD_SLOTS],
    /// Stubs with no indexable top-level method constraint — always candidates on this dimension.
    always: CandidateBits,
}

impl Dimension for MethodDimension {
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits) {
        out.copy_from(&self.always);
        self.slots[method_slot(request.method)]
            .iter()
            .for_each(|i| out.set(*i));
    }

    fn prunes(&self) -> bool {
        self.slots.iter().any(|s| !s.is_empty())
    }
}

/// Which relation a literal anchor requires between the pattern and the path.
///
/// `Copy` because [`LiteralSet::mark`] reads one out of `entries` through `&self` on every match —
/// see the field doc there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiteralKind {
    Contains,
    StartsWith,
    EndsWith,
}

/// One case class's Aho-Corasick automaton, plus the (stub, relation) each pattern id speaks for.
struct LiteralSet {
    ac: AhoCorasick,
    /// Indexed by pattern id — 1:1 with the patterns handed to `AhoCorasick::build`, so identical
    /// literals from two stubs get two ids, not one (mirrors [`MultiRegex::pattern_stubs`]).
    entries: Vec<(usize, LiteralKind)>,
}

impl LiteralSet {
    /// Set a bit for every stub whose literal anchor holds against `path`, in one overlapping pass
    /// over the haystack.
    ///
    /// `startsWith`/`endsWith` are not separate automata: a hit's *position* proves which relation
    /// holds, since `find_overlapping_iter` already reports the match span. `contains` needs no
    /// further check — the substring itself is the constraint.
    fn mark(&self, path: &str, out: &mut CandidateBits) {
        for m in self.ac.find_overlapping_iter(path) {
            let (stub, kind) = self.entries[m.pattern().as_usize()];
            let hit = match kind {
                LiteralKind::Contains => true,
                LiteralKind::StartsWith => m.start() == 0,
                LiteralKind::EndsWith => m.end() == path.len(),
            };
            if hit {
                out.set(stub);
            }
        }
    }
}

/// Build one case class's automaton, falling back that class's stubs wholesale on failure.
///
/// Unlike the regex dimension's per-pattern retry ([`build_case_class`]), a literal string cannot
/// be individually invalid the way a regex can — there is no analogue of "this one pattern is a
/// typo". A build failure here is only ever aho-corasick's own construction limits being overrun
/// in aggregate, so there is nothing to retry pattern-by-pattern: every entry in this case class
/// falls back to `always_bits`, which over-approximates it safely (Stage 2 still evaluates it).
fn build_literal_class(
    entries: Vec<(usize, String, LiteralKind)>,
    case_insensitive: bool,
) -> (Option<LiteralSet>, Vec<usize>) {
    if entries.is_empty() {
        return (None, Vec::new());
    }
    let patterns: Vec<&str> = entries.iter().map(|(_, p, _)| p.as_str()).collect();
    match AhoCorasick::builder()
        .match_kind(AcMatchKind::Standard)
        .ascii_case_insensitive(case_insensitive)
        .build(&patterns)
    {
        Ok(ac) => {
            let entries = entries
                .into_iter()
                .map(|(stub, _, kind)| (stub, kind))
                .collect();
            (Some(LiteralSet { ac, entries }), Vec::new())
        }
        Err(e) => {
            tracing::warn!(
                stubs = entries.len(),
                case_insensitive,
                error = %e,
                "literal dimension: multi-pattern build failed for the whole set; all its stubs \
                 fall back to full predicate evaluation"
            );
            (None, entries.into_iter().map(|(stub, _, _)| stub).collect())
        }
    }
}

/// A stub's required literal-path constraint: the text, the relation it requires, and which case
/// class it belongs to.
struct LiteralAnchor<'a> {
    text: &'a str,
    kind: LiteralKind,
    case_sensitive: bool,
}

/// A single predicate's literal anchor, if it is an indexable required constraint.
///
/// Deliberately narrower than the evaluator, same rationale as [`regex_anchor`]: only a **string**
/// literal qualifies. A non-string value is still a real constraint (the evaluator renders it via
/// `to_string`) — just not one worth the eligibility bookkeeping — so it falls to `always_bits`,
/// which over-approximates it safely.
fn literal_anchor(pred: &Predicate) -> Option<LiteralAnchor<'_>> {
    if !is_value_preserving(&pred.parameters) {
        return None;
    }
    let (fields, kind) = match &pred.operation {
        PredicateOperation::Contains(fields) => (fields, LiteralKind::Contains),
        PredicateOperation::StartsWith(fields) => (fields, LiteralKind::StartsWith),
        PredicateOperation::EndsWith(fields) => (fields, LiteralKind::EndsWith),
        _ => return None,
    };
    match fields.get("path") {
        Some(serde_json::Value::String(text)) => Some(LiteralAnchor {
            text,
            kind,
            case_sensitive: pred.parameters.case_sensitive == Some(true),
        }),
        _ => None,
    }
}

/// The literal dimension (issue #710) — supersedes the path dimension's old prefix/contains
/// linear-bucket walks, and additionally indexes `endsWith`, which was never indexed before it.
///
/// # One automaton per case class, not one per (case class, relation)
///
/// A stub is eligible on `contains`/`startsWith`/`endsWith`, and case-sensitive/insensitive, which
/// suggests up to six automata. This type builds only two — one per case class — because which
/// relation a hit proves is a property of *where* it landed, not of which automaton found it:
/// `find_overlapping_iter` already reports the match span, so [`LiteralSet::mark`] resolves
/// `startsWith` as `m.start() == 0` and `endsWith` as `m.end() == path.len()` after the fact, with
/// `contains` needing no further check. One overlapping pass per case class answers all three.
///
/// # Why `MatchKind::Standard` + `find_overlapping_iter`, not a single-winner search
///
/// A non-overlapping search, or a search under `LeftmostFirst`/`LeftmostLongest`, reports only the
/// winning pattern at a position and moves on — so of two literals that both match the same path
/// (e.g. `startsWith "/api"` and `startsWith "/api/v1"` against `/api/v1/x`), only one would be
/// reported and the other's stub would silently drop out of the candidate set: an
/// under-approximation, i.e. a silent no-match in production. This is exactly the bug class #709's
/// regex dimension guards against with `MatchKind::All`; `overlapping_literals_are_all_candidates`
/// below is its regression test here. `find_overlapping_iter` requires `MatchKind::Standard` (the
/// non-overlapping, single-winner kinds cannot drive it at all) — it is the only combination that
/// is a sound, all-reporting search.
///
/// # Case semantics — an exact mirror, not merely an over-approximation
///
/// `predicates/mod.rs:255-277` dispatches `case_sensitive` to either an exact-byte compare or to
/// `contains_ignore_ascii_case`/`starts_with_ignore_ascii_case`/`ends_with_ignore_ascii_case`
/// (`mod.rs:123-142`) — all of which are byte-level ASCII folds (`eq_ignore_ascii_case`), not
/// Unicode. `AhoCorasick::ascii_case_insensitive(true)` is that exact same byte-level ASCII fold,
/// so the CI automaton matches the evaluator's default precisely and the CS automaton (flag off)
/// matches its exact-byte compare precisely — neither over- nor under-approximates. This is
/// UNLIKE the sibling regex dimension, whose `matches` predicate folds with *Unicode*
/// case-insensitivity ([`RegexDimension`]'s doc); do not carry that automaton-per-flag reasoning
/// over here without re-checking which fold the predicate in question actually uses.
struct LiteralDimension {
    /// The default class: `ascii_case_insensitive(true)`, matching the evaluator's default fold.
    insensitive: Option<LiteralSet>,
    /// `caseSensitive: true`, matching the evaluator's exact-byte compare.
    sensitive: Option<LiteralSet>,
    /// Stubs with no indexable top-level literal constraint, an empty-string literal (an
    /// unconditional pass in the evaluator — see `empty_literal_always_matches`), or whose case
    /// class's automaton build failed — always candidates on this dimension.
    always: CandidateBits,
}

impl Dimension for LiteralDimension {
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits) {
        out.copy_from(&self.always);
        for s in [&self.insensitive, &self.sensitive].into_iter().flatten() {
            s.mark(request.path, out);
        }
    }

    fn prunes(&self) -> bool {
        self.insensitive.is_some() || self.sensitive.is_some()
    }
}

/// Ceiling on the NFA a single multi-pattern build may occupy. Matches the `regex` crate's own
/// default `size_limit`, so a pattern set this rejects is one whose patterns the evaluator would
/// have struggled to compile individually anyway — and rejection is only ever a fall back to
/// `always_bits`, never an error.
const NFA_SIZE_LIMIT: usize = 10 * (1 << 20);

/// Lazy-DFA budgets, raised well above the defaults on purpose.
///
/// These do not gate whether a build *succeeds* — they decide which engine the meta regex picks at
/// search time. A multi-pattern set that overruns them silently degrades to the PikeVM
/// (rust-lang/regex#881), which is the slow path this dimension exists to avoid; the whole point is
/// one prefiltered automaton pass, so buy the cache rather than the fallback.
const DFA_SIZE_LIMIT: usize = 32 * (1 << 20);
const HYBRID_CACHE_CAPACITY: usize = 16 * (1 << 20);

/// One multi-pattern automaton plus the stub each pattern id speaks for.
struct MultiRegex {
    re: meta::Regex,
    /// Indexed by pattern id. One entry per pattern handed to `build_many`, so this is 1:1 with
    /// the automaton's pattern ids — identical patterns from two stubs get two ids, not one.
    pattern_stubs: Vec<usize>,
}

thread_local! {
    /// Scratch [`PatternSet`] for [`MultiRegex::mark`]. regex-automata recommends reusing a
    /// `PatternSet` across searches (clearing between calls) rather than allocating one per call.
    /// `StubSnapshot`/dimensions are `Arc`-shared and read concurrently, so the reuse buffer must
    /// be per-thread. Matching runs synchronously in both of its homes (inline on a tokio worker,
    /// or on a `spawn_blocking` thread for inject/scenario snapshots) and never holds this borrow
    /// across an `.await`, so the scoped borrow is sound in both (issue #731).
    static PATTERN_SCRATCH: RefCell<PatternSet> = RefCell::new(PatternSet::new(0));
}

impl MultiRegex {
    /// Set a bit for every stub whose pattern matches `path`, in one pass over the haystack.
    fn mark(&self, path: &str, out: &mut CandidateBits) {
        PATTERN_SCRATCH.with_borrow_mut(|set| {
            // `which_overlapping_matches` needs a `PatternSet` sized for this automaton. Grow the
            // reused buffer only when this automaton has more patterns than any seen before on
            // this thread; otherwise `clear` it — a larger capacity is fine, `iter` yields only
            // the pids actually inserted by this search.
            let needed = self.re.pattern_len();
            if set.capacity() < needed {
                *set = PatternSet::new(needed);
            } else {
                set.clear();
            }
            self.re.which_overlapping_matches(&Input::new(path), set);
            for pid in set.iter() {
                out.set(self.pattern_stubs[pid.as_usize()]);
            }
        });
    }
}

/// Build one automaton over `patterns`. `case_insensitive` is the *default* for the set; inline
/// flags in a pattern still override it, exactly as they do for the evaluator's `RegexBuilder`.
fn build_multi_regex(
    patterns: &[&str],
    case_insensitive: bool,
) -> Result<meta::Regex, Box<meta::BuildError>> {
    meta::Regex::builder()
        .syntax(syntax::Config::new().case_insensitive(case_insensitive))
        .configure(
            meta::Config::new()
                // Load-bearing, and the whole reason this dimension is equivalent to per-pattern
                // `is_match`. `which_overlapping_matches` reports *every* matching pattern only
                // under `MatchKind::All`; under the default (`LeftmostFirst`) it reports the
                // leftmost-first winner and stops, so of two patterns that both match a path only
                // one would be reported and the other's stub would be dropped from the candidate
                // set — an under-approximation, i.e. a silent no-match in production.
                .match_kind(MatchKind::All)
                .nfa_size_limit(Some(NFA_SIZE_LIMIT))
                .dfa_size_limit(Some(DFA_SIZE_LIMIT))
                .hybrid_cache_capacity(HYBRID_CACHE_CAPACITY),
        )
        .build_many(patterns)
        .map_err(Box::new)
}

/// A log-safe rendering of an operator-supplied pattern.
///
/// A rejected pattern is frequently rejected *because* it is enormous, so cap what reaches the log
/// line — and cut on a char boundary, since patterns may be non-ASCII.
fn pattern_preview(p: &str) -> String {
    const MAX: usize = 80;
    if p.len() <= MAX {
        return p.to_owned();
    }
    let cut = (0..=MAX)
        .rev()
        .find(|i| p.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}… ({} bytes total)", &p[..cut], p.len())
}

/// Assemble the automaton for one case class, returning the stubs that must fall back to
/// `always_bits` because no automaton can speak for them.
///
/// `build_many` fails the *whole set* on the first pattern it rejects, so one typo'd or oversized
/// pattern would otherwise cost every other regex stub its pruning. The happy path pays nothing for
/// that: only on failure do we re-validate pattern-by-pattern, drop the offenders, and rebuild.
///
/// Falling back is always sound in the direction that matters: `always_bits` over-approximates, so
/// the stub simply keeps the pre-#709 behaviour of being fully evaluated by Stage 2. (Note the
/// rejected set is what *this* multi-pattern build rejects, which is not necessarily what the
/// evaluator's own `Regex::new` would reject — the soundness argument rests on
/// over-approximation, not on parity with the evaluator.)
fn build_case_class(
    entries: Vec<(usize, String)>,
    case_insensitive: bool,
) -> (Option<MultiRegex>, Vec<usize>) {
    let assemble = |entries: &[(usize, String)]| -> Option<MultiRegex> {
        if entries.is_empty() {
            return None;
        }
        let patterns: Vec<&str> = entries.iter().map(|(_, p)| p.as_str()).collect();
        let re = build_multi_regex(&patterns, case_insensitive).ok()?;
        Some(MultiRegex {
            re,
            pattern_stubs: entries.iter().map(|(stub, _)| *stub).collect(),
        })
    };

    if entries.is_empty() {
        return (None, Vec::new());
    }
    if let Some(m) = assemble(&entries) {
        return (Some(m), Vec::new());
    }

    // Something in the set is unbuildable. Find it by pattern, so its neighbours keep their
    // pruning and the operator learns which stub lost its index and why.
    let mut ok: Vec<(usize, String)> = Vec::new();
    let mut fallback: Vec<usize> = Vec::new();
    for (stub, pattern) in entries {
        match build_multi_regex(&[pattern.as_str()], case_insensitive) {
            Ok(_) => ok.push((stub, pattern)),
            Err(e) => {
                tracing::warn!(
                    stub,
                    pattern = %pattern_preview(&pattern),
                    case_insensitive,
                    error = %e,
                    "regex dimension: pattern rejected by the multi-pattern build; this stub falls \
                     back to full predicate evaluation"
                );
                fallback.push(stub);
            }
        }
    }

    // With every individually-bad pattern removed, retry. If none was individually bad, the set
    // only overruns the limits in aggregate and rebuilding the same patterns would fail again.
    if !fallback.is_empty()
        && let Some(m) = assemble(&ok)
    {
        return (Some(m), fallback);
    }

    tracing::warn!(
        stubs = ok.len(),
        case_insensitive,
        "regex dimension: multi-pattern build failed for the whole set (aggregate size); all its \
         stubs fall back to full predicate evaluation"
    );
    fallback.extend(ok.into_iter().map(|(stub, _)| stub));
    (None, fallback)
}

/// A stub's required path-regex constraint: the pattern, and which case class it belongs to.
struct RegexAnchor<'a> {
    pattern: &'a str,
    case_sensitive: bool,
}

/// A single predicate's path-regex anchor, if it is an indexable required constraint.
///
/// Deliberately narrower than the evaluator: only a **string** pattern qualifies. The evaluator
/// renders a non-string value with `to_string` and matches that (`fields.rs`), so such a predicate
/// is still a real constraint — just not one worth the eligibility bookkeeping. It falls to
/// `always_bits`, which over-approximates it safely.
fn regex_anchor(pred: &Predicate) -> Option<RegexAnchor<'_>> {
    if !is_value_preserving(&pred.parameters) {
        return None;
    }
    let PredicateOperation::Matches(fields) = &pred.operation else {
        return None;
    };
    match fields.get("path") {
        Some(serde_json::Value::String(pattern)) => Some(RegexAnchor {
            pattern,
            case_sensitive: pred.parameters.case_sensitive == Some(true),
        }),
        _ => None,
    }
}

/// The regex dimension (issue #709).
///
/// Answers "which of these N path patterns match?" in a single pass per case class, rather than the
/// N independent `is_match` calls Stage 2 would otherwise run — the meta engine extracts the
/// patterns' required literals and prefilters with memchr/Teddy, so most requests never enter an
/// automaton state.
///
/// # Why two automata
///
/// `matches` does **not** fold like the string operators, and this is the one thing to get right
/// here. `fields.rs` builds its regex as `cached_regex(pattern, !case_sensitive)`, so the default
/// (no `caseSensitive`) is `RegexBuilder::case_insensitive(true)` — the regex crate's *Unicode*
/// fold, not the ASCII `eq_ignore_ascii_case` that [`fold`] above mirrors. The case flag is a
/// per-automaton syntax config, not something that can be folded into a pattern string, so the two
/// classes get one automaton each. Inline flags (`(?i)`/`(?-i)`) override the automaton default
/// per pattern exactly as they override `RegexBuilder`'s, so they need no special handling.
struct RegexDimension {
    /// The default class: `case_insensitive(true)`, matching `cached_regex(p, true)`.
    insensitive: Option<MultiRegex>,
    /// `caseSensitive: true`, matching `cached_regex(p, false)`.
    sensitive: Option<MultiRegex>,
    /// Stubs with no indexable top-level path-regex constraint, plus any whose pattern no
    /// automaton could take — always candidates on this dimension.
    always: CandidateBits,
}

impl Dimension for RegexDimension {
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits) {
        out.copy_from(&self.always);
        for m in [&self.insensitive, &self.sensitive].into_iter().flatten() {
            m.mark(request.path, out);
        }
    }

    fn prunes(&self) -> bool {
        self.insensitive.is_some() || self.sensitive.is_some()
    }
}

/// The structural hash of a stub predicate's eligible `deepEquals`-on-`body` constraint (#708).
///
/// Eligible (conservative first cut): a top-level `deepEquals` whose `body` field is an object or
/// array (a scalar `body` compares against the raw request string, not the parse — `fields.rs`),
/// that is safely indexable ([`is_safely_indexable`]: no `except`, no selector, not
/// `caseSensitive: true`) and not `keyCaseSensitive: true`, and whose expected body has no JSON-ish
/// string leaf ([`structural_hash`] → `Some`). Anything else → `None` → `always_bits`.
///
/// Other fields on the same `deepEquals`, and the stub's other predicates, are further required
/// constraints; ignoring them here only over-approximates (Stage-2 still checks them), exactly as
/// the path/regex/literal classifiers index only their first anchor.
fn body_anchor(pred: &Predicate) -> Option<u64> {
    if !is_safely_indexable(&pred.parameters) || pred.parameters.key_case_sensitive == Some(true) {
        return None;
    }
    let PredicateOperation::DeepEquals(fields) = &pred.operation else {
        return None;
    };
    match fields.get("body") {
        Some(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            structural_hash(v)
        }
        _ => None,
    }
}

/// The deepEquals-body dimension (issue #708): collapses "which of N `deepEquals`-on-body stubs
/// equals this request body" from O(stubs × body) structural comparisons into one hash probe.
struct BodyHashDimension {
    /// `structural_hash(expected body)` → the stubs sharing it. A `Vec` bucket (not a bitset) like
    /// [`PathDimension`], since it holds only the stubs at one hash. Keyed by our own `FixedState`-
    /// derived `u64`, so the request-scoped `FastMap` hasher just re-hashes an already-good `u64`.
    by_hash: FastMap<u64, Vec<usize>>,
    /// Stubs with no eligible `deepEquals` body anchor — always candidates on this dimension.
    always: CandidateBits,
    len: usize,
}

impl Dimension for BodyHashDimension {
    fn select(&self, request: &DimensionRequest<'_>, out: &mut CandidateBits) {
        match request.body {
            // A non-JSON (or absent) body can never `deepEquals` an object/array expected body, so
            // every indexed stub is soundly excluded — only `always` stubs survive.
            None => out.copy_from(&self.always),
            Some(body) => match structural_hash(body) {
                // A JSON-ish string leaf in the request body is equivalent (both directions) to a
                // container an indexed stub may expect, which no structural hash can reconcile — so
                // this dimension must not prune this request. Widen to all; Stage-2 decides.
                None => out.copy_from(&CandidateBits::all(self.len)),
                Some(h) => {
                    out.copy_from(&self.always);
                    if let Some(ids) = self.by_hash.get(&h) {
                        ids.iter().for_each(|i| out.set(*i));
                    }
                }
            },
        }
    }

    fn prunes(&self) -> bool {
        !self.by_hash.is_empty()
    }
}

/// The multi-dimensional candidate prefilter over a stub snapshot. See the module docs.
pub(crate) struct StubIndex {
    len: usize,
    path: PathDimension,
    method: MethodDimension,
    literal: LiteralDimension,
    regex: RegexDimension,
    body: BodyHashDimension,
}

impl StubIndex {
    /// Build every dimension in one pass over the stubs, preserving ascending stub id within each
    /// bucket (so iteration stays declaration-ordered).
    fn build(stubs: &[Arc<StubState>]) -> Self {
        let len = stubs.len();
        let mut exact: FastMap<String, Vec<usize>> = FastMap::default();
        let mut path_always = CandidateBits::zeros(len);

        let mut slots: [Vec<usize>; METHOD_SLOTS] = Default::default();
        let mut method_always = CandidateBits::zeros(len);

        let mut literal_ci: Vec<(usize, String, LiteralKind)> = Vec::new();
        let mut literal_cs: Vec<(usize, String, LiteralKind)> = Vec::new();
        let mut literal_always = CandidateBits::zeros(len);

        let mut regex_ci: Vec<(usize, String)> = Vec::new();
        let mut regex_cs: Vec<(usize, String)> = Vec::new();
        let mut regex_always = CandidateBits::zeros(len);

        let mut body_by_hash: FastMap<u64, Vec<usize>> = FastMap::default();
        let mut body_always = CandidateBits::zeros(len);

        for (i, state) in stubs.iter().enumerate() {
            match classify(&state.stub) {
                Some(PathAnchor::Exact(k)) => exact.entry(k).or_default().push(i),
                None => path_always.set(i),
            }
            match state.stub.predicates.iter().find_map(method_anchor) {
                Some(m) => slots[method_slot(m)].push(i),
                None => method_always.set(i),
            }
            // Only the first literal anchor is indexed — same rule as `classify`/`regex_anchor`.
            // An empty literal is an unconditional pass in the evaluator (see
            // `empty_literal_always_matches`), so it must never reach the automaton: feeding it in
            // would ask aho-corasick to match the empty string, and even if that "worked" it would
            // be solving the wrong problem — the stub must be a candidate for every path, not just
            // ones the empty pattern happens to hit.
            match state.stub.predicates.iter().find_map(literal_anchor) {
                Some(a) if a.text.is_empty() => literal_always.set(i),
                Some(a) if a.case_sensitive => literal_cs.push((i, a.text.to_owned(), a.kind)),
                Some(a) => literal_ci.push((i, a.text.to_owned(), a.kind)),
                None => literal_always.set(i),
            }
            // Only the first regex anchor is indexed. A stub's further `matches` predicates are
            // also required, so ignoring them only ever over-approximates — Stage 2 still checks
            // them. Same rule as the path dimension's `classify`.
            match state.stub.predicates.iter().find_map(regex_anchor) {
                Some(a) if a.case_sensitive => regex_cs.push((i, a.pattern.to_owned())),
                Some(a) => regex_ci.push((i, a.pattern.to_owned())),
                None => regex_always.set(i),
            }
            // Only the first eligible deepEquals-body anchor is indexed — same first-anchor rule as
            // above; a stub's further body constraints stay required and are checked by Stage 2.
            match state.stub.predicates.iter().find_map(body_anchor) {
                Some(h) => body_by_hash.entry(h).or_default().push(i),
                None => body_always.set(i),
            }
        }

        let (literal_insensitive, literal_ci_fallback) = build_literal_class(literal_ci, true);
        let (literal_sensitive, literal_cs_fallback) = build_literal_class(literal_cs, false);
        for i in literal_ci_fallback.into_iter().chain(literal_cs_fallback) {
            literal_always.set(i);
        }

        let (insensitive, ci_fallback) = build_case_class(regex_ci, true);
        let (sensitive, cs_fallback) = build_case_class(regex_cs, false);
        for i in ci_fallback.into_iter().chain(cs_fallback) {
            regex_always.set(i);
        }

        let path = PathDimension {
            exact,
            always: path_always,
        };
        let method = MethodDimension {
            slots,
            always: method_always,
        };
        let literal = LiteralDimension {
            insensitive: literal_insensitive,
            sensitive: literal_sensitive,
            always: literal_always,
        };
        let regex = RegexDimension {
            insensitive,
            sensitive,
            always: regex_always,
        };
        let body = BodyHashDimension {
            by_hash: body_by_hash,
            always: body_always,
            len,
        };
        StubIndex {
            len,
            path,
            method,
            literal,
            regex,
            body,
        }
    }

    /// The number of stubs this index spans.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Intersect one dimension's bitset into the accumulator, seeding it on the first dimension
    /// that actually runs. Returns whether any candidate survives — `false` means the caller can
    /// stop, because intersection is monotone and no later dimension can put a bit back.
    ///
    /// The emptiness test is deliberately *inside* here, and only on the path where the dimension
    /// actually ran: `is_empty()` scans the whole bitset (16 words at 1000 stubs), so testing it
    /// after a dimension that was skipped is pure cost for an answer that cannot have changed.
    /// Returning `true` for a skipped dimension also means an unseeded (all-zeros) accumulator can
    /// never be mistaken for "everything was pruned" — the caller needs no `seeded` guard.
    ///
    /// Generic over the dimension rather than taking `&dyn Dimension`, so each call site
    /// monomorphizes to a static dispatch — see the module docs on why dimensions are concrete
    /// fields.
    fn fold_in<D: Dimension>(
        dimension: &D,
        request: &DimensionRequest<'_>,
        acc: &mut CandidateBits,
        seeded: &mut bool,
        len: usize,
    ) -> bool {
        if !dimension.prunes() {
            return true;
        }
        if *seeded {
            let mut scratch = CandidateBits::zeros(len);
            dimension.select(request, &mut scratch);
            acc.intersect_with(&scratch);
        } else {
            // `select` overwrites, so the first dimension seeds `acc` directly — no `all()` fill to
            // intersect against, and no scratch buffer.
            dimension.select(request, acc);
            *seeded = true;
        }
        !acc.is_empty()
    }

    /// Candidate stub ids for a request: the intersection of every dimension's bitset. A superset
    /// of the stubs that could match — Stage-2 does the real Mountebank evaluation on these, in the
    /// ascending (declaration) order [`CandidateBits::iter`] yields.
    /// Dimensions run cheapest-first, and only when they can actually prune:
    ///
    /// * a dimension no stub is indexed on is skipped ([`Dimension::prunes`]) — otherwise a corpus
    ///   that never constrains the method would pay the method dimension a full-width copy and
    ///   intersect to produce all-ones;
    /// * an empty accumulator short-circuits the rest. Dimensions run cheapest-first: method (a
    ///   slot lookup, no allocation) is both the cheapest and, on method-partitioned corpora, the
    ///   most selective, so its early exit can skip every later dimension's work entirely. Path
    ///   (a hashmap lookup) is next. Literal and regex both walk the haystack through an automaton,
    ///   so they run last and in that order: the literal dimension's Aho-Corasick pass is cheaper
    ///   per byte than the regex meta engine's, so it gets first crack at shrinking (or emptying)
    ///   the accumulator before the more expensive dimension has to run at all.
    ///
    /// Short-circuits through [`Self::fold_in`]'s return value rather than re-testing emptiness
    /// between dimensions: intersection is monotone, so once nothing survives no later dimension can
    /// put a bit back.
    pub(crate) fn candidates(&self, request: &DimensionRequest<'_>) -> CandidateBits {
        let mut acc = CandidateBits::zeros(self.len);
        let mut seeded = false;

        if Self::fold_in(&self.method, request, &mut acc, &mut seeded, self.len)
            && Self::fold_in(&self.path, request, &mut acc, &mut seeded, self.len)
            && Self::fold_in(&self.literal, request, &mut acc, &mut seeded, self.len)
            && Self::fold_in(&self.regex, request, &mut acc, &mut seeded, self.len)
        {
            // Body last: hashing the request body is O(body), so let the cheap path/method/automaton
            // dimensions empty the accumulator first (short-circuiting the hash away) where they can.
            Self::fold_in(&self.body, request, &mut acc, &mut seeded, self.len);
        }

        // No dimension indexes anything (e.g. every stub is a body regex): everyone is a candidate.
        if seeded {
            acc
        } else {
            CandidateBits::all(self.len)
        }
    }
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

/// The unit of stub state the match hot path reads: the stubs, the index over *those exact* stubs,
/// and the snapshot-wide precomputed gates (issue #707).
///
/// Held behind a single `ArcSwap` in [`Imposter`](super::Imposter), so one wait-free `load()` per
/// request yields all of it. Before #707 the stubs and the index lived in two `ArcSwap`s kept in
/// sync only by convention inside `mutate_stubs`; bundling them makes that invariant type-enforced
/// — a reader cannot observe an index built for a different stub vector — and costs one atomic
/// instead of two.
pub(crate) struct StubSnapshot {
    stubs: Vec<Arc<StubState>>,
    index: StubIndex,
    /// Whether any stub's predicate tree contains an `inject` predicate, anywhere (including
    /// nested under `and`/`or`/`not`). Computed once per snapshot so the request hot path can
    /// gate the bounded (spawn_blocking) matching route on it for free (issue #476).
    has_inject: bool,
    /// Whether any stub is scenario-gated (`requiredScenarioState`). The eligibility gate reads
    /// flow state during matching; on a blocking backend (Redis) that read must run off the tokio
    /// worker, so the bounded matcher offloads only when this is set — a scenario-free snapshot
    /// keeps the inline fast path even on a blocking backend (issue #475).
    has_scenario_gate: bool,
}

impl StubSnapshot {
    /// Build the index and the snapshot-wide gates for `stubs`.
    pub(crate) fn build(stubs: Vec<Arc<StubState>>) -> Self {
        let index = StubIndex::build(&stubs);
        let has_inject = stubs
            .iter()
            .any(|s| s.stub.predicates.iter().any(predicate_contains_inject));
        let has_scenario_gate = stubs
            .iter()
            .any(|s| s.stub.required_scenario_state.is_some());
        StubSnapshot {
            stubs,
            index,
            has_inject,
            has_scenario_gate,
        }
    }

    /// The stubs this snapshot describes, in declaration order.
    pub(crate) fn stubs(&self) -> &[Arc<StubState>] {
        &self.stubs
    }

    /// Whether any stub in this snapshot uses an `inject` predicate (issue #476).
    pub(crate) fn has_inject(&self) -> bool {
        self.has_inject
    }

    /// Whether any stub in this snapshot is scenario-gated (`requiredScenarioState`, issue #475).
    pub(crate) fn has_scenario_gate(&self) -> bool {
        self.has_scenario_gate
    }

    /// Candidate stub ids for a request that carries no (or a non-JSON) body — see
    /// [`StubIndex::candidates`]. The body dimension then contributes only its `always` stubs.
    pub(crate) fn candidates(&self, method: &str, path: &str) -> CandidateBits {
        self.candidates_with_body(method, path, None)
    }

    /// Candidate stub ids for a request, threading the once-per-request parsed body (#290/#708) so
    /// the deepEquals-body dimension can prune. `body_json` is `None` for a non-JSON/absent body.
    pub(crate) fn candidates_with_body(
        &self,
        method: &str,
        path: &str,
        body_json: Option<&serde_json::Value>,
    ) -> CandidateBits {
        self.index.candidates(&DimensionRequest {
            method,
            path,
            body: body_json,
        })
    }

    /// The index over these stubs (tests assert dimension-level behaviour through it).
    #[cfg(test)]
    pub(crate) fn index(&self) -> &StubIndex {
        &self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::core::Imposter;
    use crate::imposter::types::ImposterConfig;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use tracing_test::traced_test;

    fn stub_states(preds: &[Value]) -> Vec<Arc<StubState>> {
        preds
            .iter()
            .map(|p| {
                let stub = serde_json::from_value(
                    json!({ "predicates": p, "responses": [{ "is": { "statusCode": 200 } }] }),
                )
                .expect("valid stub");
                Arc::new(StubState::new(stub))
            })
            .collect()
    }

    /// A snapshot over `preds`, for dimension-level assertions.
    fn snapshot(preds: &[Value]) -> StubSnapshot {
        StubSnapshot::build(stub_states(preds))
    }

    /// The candidate ids for a request, ascending — the pre-#707 `candidates()` shape, so the
    /// existing coverage/ordering assertions read unchanged.
    fn candidate_ids(snap: &StubSnapshot, method: &str, path: &str) -> Vec<usize> {
        snap.candidates(method, path).iter().collect()
    }

    /// The candidate ids for a request with a JSON body, ascending (#708 body dimension).
    fn candidate_ids_body(
        snap: &StubSnapshot,
        method: &str,
        path: &str,
        body: &Value,
    ) -> Vec<usize> {
        snap.candidates_with_body(method, path, Some(body))
            .iter()
            .collect()
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
            json!([{"matches": {"path": "^/re[0-9]+$"}}]), // 4 regex -> regex dimension (#709)
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

    // The path dimension genuinely narrows (excludes non-matching anchored stubs) yet never drops a
    // stub the linear scan would consider (always-bits + matching anchors are all present).
    // Stub 6 is method-only (`equals {method: POST}`), so a GET request now prunes it on the method
    // dimension — the #707 pruning the path dimension alone could never do. Stub 4 is a path regex,
    // which #709's regex dimension prunes for a path its pattern cannot match.
    #[test]
    fn stub_index_narrows_and_covers() {
        let snap = snapshot(&corpus());
        let cands = candidate_ids(&snap, "GET", "/exact");

        // Narrowing: the prefix (2) and method+path-/mp (11) anchored stubs cannot match /exact,
        // so they are excluded.
        assert!(!cands.contains(&2), "prefix /pre stub excluded for /exact");
        assert!(
            !cands.contains(&11),
            "method+path /mp stub excluded for /exact"
        );
        assert!(
            !cands.contains(&6),
            "POST-only stub excluded for a GET request (method dimension, #707)"
        );
        assert!(
            !cands.contains(&4),
            "regex stub ^/re[0-9]+$ excluded for /exact (regex dimension, #709)"
        );

        // Coverage: both exact stubs (case-insensitive collision) and every stub no dimension can
        // index remain candidates.
        assert!(
            cands.contains(&0) && cands.contains(&1),
            "exact stubs present"
        );
        for fb in [5, 7, 8, 9, 10, 12] {
            assert!(
                cands.contains(&fb),
                "un-indexable stub {fb} must always be a candidate"
            );
        }
        // Ascending + deduped so Stage-2 preserves declaration order.
        let mut sorted = cands.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(cands, sorted, "candidates must be ascending and deduped");
    }

    // AC4: the method dimension collapses the candidate set — a request's method prunes every stub
    // anchored to a different one. This is the framework's proof: before #707 all four stubs were
    // candidates for every request, because the path dimension cannot see the method.
    #[test]
    fn method_dimension_collapses_candidates() {
        let snap = snapshot(&[
            json!([{"equals": {"method": "GET"}}]),
            json!([{"equals": {"method": "POST"}}]),
            json!([{"equals": {"method": "PUT"}}]),
            json!([{"equals": {"method": "DELETE"}}]),
        ]);
        for (i, m) in ["GET", "POST", "PUT", "DELETE"].iter().enumerate() {
            let c = snap.candidates(m, "/anything");
            assert_eq!(c.count(), 1, "{m}: exactly one candidate survives");
            assert!(c.contains(i), "{m}: the {m}-anchored stub is the survivor");
        }
        // A method no stub anchors prunes everything — and the early-exit path returns empty.
        assert_eq!(snap.candidates("PATCH", "/anything").count(), 0);
        // Case-insensitive, per Mountebank's default comparison.
        assert!(snap.candidates("get", "/anything").contains(0));
    }

    // AC6 (the soundness rule): a stub whose method constraint the dimension cannot index must stay
    // a candidate for EVERY method. A dimension may only exclude what it can prove cannot match —
    // anything else belongs in always_bits.
    #[test]
    fn ineligible_method_shapes_are_always_candidates() {
        let snap = snapshot(&[
            json!([{"equals": {"method": "GET"}}]), // 0 indexable
            json!([{"or": [{"equals": {"method": "GET"}}, {"equals": {"method": "POST"}}]}]), // 1 or
            json!([{"not": {"equals": {"method": "GET"}}}]), // 2 not
            json!([{"and": [{"equals": {"method": "GET"}}, {"equals": {"path": "/x"}}]}]), // 3 and
            json!([{"equals": {"method": "GET"}, "except": "X"}]), // 4 except rewrites the value
            json!([{"equals": {"method": "GET"}, "caseSensitive": true}]), // 5 caseSensitive
            json!([{"matches": {"method": "^G"}}]),          // 6 regex
            // 7: the evaluator stringifies a non-string value and compares it, so it IS a real
            // constraint — just not one this dimension indexes.
            json!([{"equals": {"method": 7}}]),
            json!([{"equals": {"method": "GET"}, "jsonpath": {"selector": "$.id"}}]), // 8 selector
            json!([]),                                                                // 9 match-all
        ]);
        // PATCH matches no indexable anchor, so only always-bits stubs may survive.
        let c = snap.candidates("PATCH", "/x");
        assert!(!c.contains(0), "the indexable GET stub must be pruned");
        for i in [1, 2, 3, 4, 5, 6, 7, 8, 9] {
            assert!(
                c.contains(i),
                "stub {i} is un-indexable → always a candidate"
            );
        }
    }

    // The `selector` arm of the shared `is_safely_indexable` gate, for the path dimension. Without
    // a test, inverting or dropping that check would be an *under*-approximation — a silently
    // pruned stub — which is the one failure mode the index must never have.
    #[test]
    fn selector_scoped_path_predicates_are_always_candidates() {
        let snap = snapshot(&[
            json!([{"equals": {"path": "/a"}, "jsonpath": {"selector": "$.id"}}]), // 0 selector
            json!([{"equals": {"path": "/a"}, "except": "X"}]),                    // 1 except
            json!([{"equals": {"path": "/a"}, "caseSensitive": true}]), // 2 caseSensitive
            json!([{"equals": {"path": "/a"}}]),                        // 3 indexable
        ]);
        // A path no anchor matches: only stubs the dimension cannot index may survive.
        let c = snap.candidates("GET", "/other");
        for i in [0, 1, 2] {
            assert!(
                c.contains(i),
                "stub {i} is un-indexable → always a candidate"
            );
        }
        assert!(!c.contains(3), "the indexable /a stub must be pruned");
    }

    // The index must fold case EXACTLY as the evaluator does (ASCII), not merely conservatively.
    // Unicode `to_lowercase` is length-changing and context-sensitive, so it breaks the prefix and
    // substring relations the path dimension relies on: the evaluator matches `startsWith "/ΟΣ"`
    // against `/ΟΣΑ` (its ASCII fold leaves Greek untouched), but a Unicode fold maps the anchor's
    // trailing Σ to a final sigma (`/ος`) that `"/οσα"` does not start with — pruning a stub that
    // does match. Regression test for that class of silent no-match.
    #[test]
    fn non_ascii_case_folding_matches_the_evaluator() {
        let imp = imposter(&[
            json!([{"startsWith": {"path": "/ΟΣ"}}]), // 0 Greek sigma: Unicode fold breaks the prefix
            json!([{"contains": {"path": "ΑΣ"}}]),    // 1 the same trap via contains
            json!([{"equals": {"path": "/İ"}}]),      // 2 dotted capital I lowercases to two chars
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [
            ("GET", "/ΟΣΑ"),
            ("GET", "/ΟΣ"),
            ("GET", "/οσα"),
            ("GET", "/xΑΣy"),
            ("GET", "/İ"),
            ("GET", "/i̇"),
        ] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }
    }

    // An unusual method shares the `Other` slot with every other unusual method. That is coarser,
    // not wrong: the dimension over-includes and verification decides. Guards against a slot scheme
    // that silently drops methods outside the named set.
    #[test]
    fn unnamed_methods_share_the_other_slot_soundly() {
        let snap = snapshot(&[
            json!([{"equals": {"method": "TRACE"}}]),
            json!([{"equals": {"method": "CONNECT"}}]),
            json!([{"equals": {"method": "GET"}}]),
        ]);
        // Both unusual stubs are candidates for either unusual method (over-approximation)...
        for m in ["TRACE", "CONNECT"] {
            let c = snap.candidates(m, "/x");
            assert!(c.contains(0) && c.contains(1), "{m}: Other-slot stubs kept");
            assert!(!c.contains(2), "{m}: the GET stub is still pruned");
        }
        // ...but full verification still returns only the truly matching one.
        let imp = imposter(&[
            json!([{"equals": {"method": "TRACE"}}]),
            json!([{"equals": {"method": "CONNECT"}}]),
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "CONNECT",
                "/x",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            Some(1)
        );
    }

    // AC1: one load yields the stubs and an index built over those exact stubs. The two cannot
    // diverge — there is no second ArcSwap to tear against — and that must survive mutation.
    #[test]
    fn snapshot_stubs_and_index_are_one_unit() {
        let imp = imposter(&[json!([{"equals": {"path": "/a"}}])]);
        for n in 1..6usize {
            let snap = imp.snapshot();
            assert_eq!(
                snap.stubs().len(),
                snap.index().len(),
                "index spans exactly the stubs it was loaded with"
            );
            // Every candidate id must be a valid index into the same load's stub vector.
            let c = snap.candidates("GET", "/a");
            assert!(c.iter().all(|i| i < snap.stubs().len()));
            drop(snap);

            let stub = serde_json::from_value(json!({
                "predicates": [{"equals": {"path": format!("/p{n}")}}],
                "responses": [{ "is": { "statusCode": 200 } }]
            }))
            .expect("valid stub");
            imp.add_stub(stub, None);
        }
        let snap = imp.snapshot();
        assert_eq!(snap.stubs().len(), 6);
        assert_eq!(snap.index().len(), 6);
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

    /// A randomized stub corpus spanning every dimension the index prunes on (method, path, and —
    /// since #709 — path regexes) and every shape it must *not* prune on (body/exists regexes,
    /// or/not/and, caseSensitive, except, selector, non-string patterns, invalid patterns, empty).
    /// Seeded, so a differential failure is reproducible from the seed alone.
    fn random_corpus(rng: &mut StdRng, n: usize) -> Vec<Value> {
        const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "TRACE"];
        const SEGS: &[&str] = &["/a", "/b", "/api/v1", "/api/v2", "/x/y", "/mid"];
        (0..n)
            .map(|_| {
                let seg = SEGS[rng.gen_range(0..SEGS.len())];
                let m = METHODS[rng.gen_range(0..METHODS.len())];
                match rng.gen_range(0..32) {
                    // Indexable on both dimensions (one predicate, two fields).
                    0 => json!([{"equals": {"method": m, "path": seg}}]),
                    // Indexable on both dimensions (two separate top-level predicates).
                    1 => json!([{"equals": {"method": m}}, {"equals": {"path": seg}}]),
                    // Method-only: prunable by method, always-candidate on path.
                    2 => json!([{"equals": {"method": m}}]),
                    // Path-only: prunable by path, always-candidate on method.
                    3 => json!([{"equals": {"path": seg}}]),
                    4 => json!([{"startsWith": {"path": seg}}]),
                    5 => json!([{"contains": {"path": seg}}]),
                    // Indexable on the regex dimension (#709): a top-level `matches` on `path`.
                    6 => json!([{"matches": {"path": format!("^{seg}[0-9]*$")}}]),
                    // Case-sensitive regex — the other automaton.
                    7 => json!([{"matches": {"path": format!("^{seg}$")}, "caseSensitive": true}]),
                    // Inline flags must override the automaton's default in both directions.
                    8 => json!([{"matches": {"path": format!("(?i)^{seg}[0-9]*$")}}]),
                    9 => json!([{"matches": {"path": format!("(?-i)^{seg}[0-9]*$")}}]),
                    // Unanchored regex — `which_overlapping_matches` must agree with `is_match`.
                    10 => json!([{"matches": {"path": "[0-9]+"}}]),
                    // Regex shapes the dimension must NOT index — all must stay always-candidates.
                    11 => json!([{"matches": {"path": format!("^{seg}$"), "except": "X"}}]),
                    12 => json!([{"matches": {"path": format!("^{seg}$")}, "jsonpath": {"selector": "$.id"}}]),
                    13 => json!([{"matches": {"path": 7}}]),
                    14 => json!([{"matches": {"path": "^/unclosed["}}]),
                    15 => json!([{"or": [{"equals": {"method": m}}, {"matches": {"path": format!("^{seg}$")}}]}]),
                    16 => json!([{"not": {"equals": {"path": seg}}}]),
                    17 => json!([{"equals": {"method": m}, "caseSensitive": true}]),
                    18 => json!([{"equals": {"method": m}, "except": "X"}]),
                    // Literal shapes (#710). `endsWith` was never indexed before it, and the
                    // case-sensitive and empty-literal shapes are the edges the issue names.
                    20 => json!([{"endsWith": {"path": seg}}]),
                    21 => json!([{"startsWith": {"path": seg}, "caseSensitive": true}]),
                    22 => json!([{"contains": {"path": seg}, "caseSensitive": true}]),
                    23 => json!([{"endsWith": {"path": seg}, "caseSensitive": true}]),
                    // An empty literal is an unconditional pass in the evaluator — it must never
                    // be pruned.
                    24 => json!([{"contains": {"path": ""}}]),
                    25 => json!([{"startsWith": {"path": ""}}]),
                    // A deliberately broad literal, drawn from the same SEGS as the anchored
                    // shapes so it frequently OVERLAPS one of them on the same path — the shape
                    // that distinguishes an all-reporting automaton from a single-winner one.
                    26 => json!([{"contains": {"path": "/"}}]),
                    27 => json!([{"startsWith": {"path": &seg[..2.min(seg.len())]}}]),
                    // A deliberately BROAD path regex, drawn from the same `SEGS` as the anchored
                    // shapes above so it frequently overlaps one of them on the same path. Two
                    // patterns matching one path is the only shape that can distinguish
                    // `MatchKind::All` from the meta engine's `LeftmostFirst` default, and without
                    // a shape that reliably produces it this oracle cannot see that whole class of
                    // under-approximation — it passed against exactly that bug during #709.
                    19 => json!([{"matches": {"path": format!("^{seg}")}}]),
                    // deepEquals-body (#708): eligible object/array bodies the body-hash dimension
                    // indexes, drawn from the same templates the request generator sends so they
                    // frequently match — the shape that exercises the dimension, not just its always
                    // set. `caseSensitive`/json-in-string are the carve-outs that must stay always.
                    28 => json!([{"deepEquals": {"body": {"k": 1}}}]),
                    29 => json!([{"deepEquals": {"body": [1, 2]}}]),
                    30 => json!([{"deepEquals": {"body": {"n": {"a": 1}}}}]),
                    31 => json!([{"deepEquals": {"body": {"k": 1}}, "caseSensitive": true}]),
                    _ => json!([]),
                }
            })
            .collect()
    }

    // AC2 (the load-bearing correctness gate): over a randomized corpus, the indexed path must
    // return exactly the stub the linear oracle returns — same index, same first-match-wins order —
    // for every request. This is a *characterization* gate: it holds for the pre-#707 index too, and
    // must keep holding through the snapshot/bitset refactor and every dimension added on top of it
    // (#708/#709/#710). Any dimension that under-approximates (prunes a stub that could match)
    // fails here.
    #[test]
    fn differential_index_matches_linear_oracle() {
        const METHODS: &[&str] = &[
            "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "TRACE", "get",
        ];
        const PATHS: &[&str] = &[
            "/a",
            "/b",
            "/A",
            "/api/v1",
            "/api/v2",
            "/api/v1/deep",
            "/x/y",
            "/x-mid-y",
            "/mid",
            "/zzz",
            "/a0",
            "/GETX",
        ];
        let mut rng = StdRng::seed_from_u64(0x0707_5EED);
        let no_headers: HashMap<String, String> = HashMap::new();

        // Several independent corpora, so the assertion isn't hostage to one lucky stub layout.
        for corpus_n in 0..8 {
            let imp = imposter(&random_corpus(&mut rng, 40));
            for _ in 0..1250 {
                let m = METHODS[rng.gen_range(0..METHODS.len())];
                let p = PATHS[rng.gen_range(0..PATHS.len())];
                // Mix non-JSON, absent, and several JSON bodies — exact matches, a type-coercion
                // variant, an array, a JSON-in-string leaf (the bail path), and a nested object —
                // so the body-hash dimension (#708) is exercised against the linear oracle.
                let body = match rng.gen_range(0..8) {
                    0 => Some("ping"),
                    1 => Some(r#"{"k":1}"#),
                    2 => Some(r#"{"k":"1"}"#),
                    3 => Some(r#"[1,2]"#),
                    4 => Some(r#"{"n":{"a":1}}"#),
                    5 => Some(r#"{"j":"{\"x\":1}"}"#),
                    6 => None,
                    _ => None,
                };
                let query = if rng.gen_bool(0.25) {
                    Some("a=1")
                } else {
                    None
                };

                let linear =
                    idx(imp.find_matching_stub_linear(m, p, &no_headers, query, body, None, None));
                let indexed = idx(imp.find_matching_stub_with_client(
                    m,
                    p,
                    &no_headers,
                    query,
                    body,
                    None,
                    None,
                ));
                assert_eq!(
                    indexed, linear,
                    "index diverged from linear oracle: corpus {corpus_n}, {m} {p} q={query:?} b={body:?}"
                );
            }
        }
    }

    // ---- issue #710: the literal dimension -----------------------------------------------

    // AC1: one pass answers which of N literals match — a request targeting one stub out of many
    // literal-anchored stubs must leave a small candidate set, not all N. Before #710 the prefix
    // and contains buckets were walked linearly, so the prefilter itself scaled with the number of
    // distinct literals even though one stub matched.
    #[test]
    fn literal_dimension_collapses_candidates() {
        let mut preds: Vec<Value> = Vec::new();
        for i in 0..200 {
            preds.push(json!([{"startsWith": {"path": format!("/pre{i}/")}}]));
            preds.push(json!([{"contains": {"path": format!("~mid{i}~")}}]));
            preds.push(json!([{"endsWith": {"path": format!("/end{i}")}}]));
        }
        let snap = snapshot(&preds);

        // A startsWith target: only its own stub survives.
        let c = snap.candidates("GET", "/pre199/detail");
        assert_eq!(c.count(), 1, "one startsWith stub survives");
        assert!(c.contains(199 * 3));

        // A contains target.
        let c = snap.candidates("GET", "/x~mid150~y");
        assert_eq!(c.count(), 1, "one contains stub survives");
        assert!(c.contains(150 * 3 + 1));

        // An endsWith target — a kind the index could not see at all before #710.
        let c = snap.candidates("GET", "/whatever/end42");
        assert_eq!(c.count(), 1, "one endsWith stub survives");
        assert!(c.contains(42 * 3 + 2));

        // A path no literal matches prunes everything.
        assert_eq!(snap.candidates("GET", "/nothing").count(), 0);
    }

    // The #709 lesson, applied: a multi-pattern automaton must report EVERY matching pattern, not
    // just one. `MatchKind::LeftmostFirst`/`LeftmostLongest` + a non-overlapping search would
    // report a single winner per position and silently prune the others' stubs — an
    // under-approximation. Only `MatchKind::Standard` + an overlapping search reports all.
    // Every literal here is matched by the one path, in all three kinds and both nesting shapes.
    #[test]
    fn overlapping_literals_are_all_candidates() {
        let snap = snapshot(&[
            json!([{"startsWith": {"path": "/api"}}]), // 0 prefix of the path
            json!([{"startsWith": {"path": "/api/v1"}}]), // 1 longer prefix — overlaps 0
            json!([{"contains": {"path": "api"}}]),    // 2 substring — overlaps 0 and 1
            json!([{"contains": {"path": "api/v1/x"}}]), // 3 longer substring
            json!([{"endsWith": {"path": "/x"}}]),     // 4 suffix
            json!([{"endsWith": {"path": "v1/x"}}]),   // 5 longer suffix — overlaps 4
            json!([{"contains": {"path": "/"}}]),      // 6 occurs many times over
        ]);
        let c = snap.candidates("GET", "/api/v1/x");
        for i in 0..7 {
            assert!(
                c.contains(i),
                "stub {i}'s literal matches /api/v1/x and must survive — every overlapping \
                 pattern must be reported"
            );
        }
    }

    // The empty literal is an unconditional pass in the evaluator: `contains_ignore_ascii_case`
    // returns true on an empty needle, and starts/ends reduce to comparing an empty slice. The
    // case-sensitive side agrees (`"x".contains("") == true`). So an empty-literal stub must be a
    // candidate for EVERY path — the one edge the issue calls out by name.
    #[test]
    fn empty_literal_always_matches() {
        let imp = imposter(&[
            json!([{"contains": {"path": ""}}]),   // 0
            json!([{"startsWith": {"path": ""}}]), // 1
            json!([{"endsWith": {"path": ""}}]),   // 2
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [("GET", "/anything"), ("GET", "/"), ("POST", "/x/y/z")] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
            assert_eq!(
                linear,
                Some(0),
                "the empty contains stub matches everything"
            );
        }
        // All three stay candidates regardless of path.
        let snap = snapshot(&[
            json!([{"contains": {"path": ""}}]),
            json!([{"startsWith": {"path": ""}}]),
            json!([{"endsWith": {"path": ""}}]),
        ]);
        let c = snap.candidates("GET", "/zzz");
        for i in 0..3 {
            assert!(
                c.contains(i),
                "empty-literal stub {i} must always be a candidate"
            );
        }
    }

    // The case flag routes to the CS or CI automaton, and the fold must be the evaluator's:
    // byte-level ASCII (`eq_ignore_ascii_case`), which is exactly aho-corasick's
    // `ascii_case_insensitive`. Asserted against the linear oracle, including the non-ASCII
    // boundary where an ASCII fold deliberately does NOT fold (the Greek-sigma trap #707 documents).
    #[test]
    fn literal_case_semantics_match_the_evaluator() {
        let imp = imposter(&[
            json!([{"startsWith": {"path": "/Pre"}}]), // 0 default => ASCII case-insensitive
            json!([{"startsWith": {"path": "/Pre2"}, "caseSensitive": true}]), // 1 case-sensitive
            json!([{"contains": {"path": "MiD"}}]),    // 2 CI contains
            json!([{"contains": {"path": "MiD2"}, "caseSensitive": true}]), // 3 CS contains
            json!([{"endsWith": {"path": "/End"}}]),   // 4 CI endsWith
            json!([{"endsWith": {"path": "/End2"}, "caseSensitive": true}]), // 5 CS endsWith
            json!([{"startsWith": {"path": "/ΟΣ"}}]),  // 6 non-ASCII: ASCII fold leaves Greek alone
            json!([{"contains": {"path": "ΑΣ"}}]),     // 7 the same via contains
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [
            ("GET", "/pre/x"),
            ("GET", "/PRE/x"),
            ("GET", "/Pre2/x"),
            ("GET", "/pre2/x"),
            ("GET", "/a-mid-b"),
            ("GET", "/a-MiD-b"),
            ("GET", "/a-MiD2-b"),
            ("GET", "/a-mid2-b"),
            ("GET", "/x/end"),
            ("GET", "/x/END"),
            ("GET", "/x/End2"),
            ("GET", "/x/end2"),
            ("GET", "/ΟΣΑ"),
            ("GET", "/οσα"),
            ("GET", "/xΑΣy"),
        ] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }
    }

    // The soundness rule for the literal dimension: every shape it cannot index must stay a
    // candidate for EVERY request. `except` rewrites the compared value, `selector` re-scopes it,
    // a non-string literal is still a real constraint (the evaluator renders it), and or/not/and
    // nesting is not a required top-level constraint.
    #[test]
    fn ineligible_literal_shapes_are_always_candidates() {
        let snap = snapshot(&[
            json!([{"startsWith": {"path": "/idx"}}]), // 0 indexable
            json!([{"startsWith": {"path": "/x"}, "except": "Y"}]), // 1 except
            json!([{"contains": {"path": "/x"}, "jsonpath": {"selector": "$.id"}}]), // 2 selector
            json!([{"or": [{"startsWith": {"path": "/x"}}, {"equals": {"path": "/y"}}]}]), // 3 or
            json!([{"not": {"contains": {"path": "/x"}}}]), // 4 not
            json!([{"and": [{"endsWith": {"path": "/x"}}, {"equals": {"method": "GET"}}]}]), // 5 and
            json!([{"contains": {"body": "ping"}}]), // 6 non-path field
            json!([{"startsWith": {"path": 7}}]),    // 7 non-string literal
        ]);
        let c = snap.candidates("GET", "/other");
        assert!(!c.contains(0), "the indexable /idx stub must be pruned");
        for i in 1..8 {
            assert!(
                c.contains(i),
                "stub {i} is un-indexable → must always be a candidate"
            );
        }
    }

    // ---- issue #709: the regex dimension -------------------------------------------------

    // AC1: the dimension answers "which of these N patterns match" in one pass — a request that
    // targets one stub out of many regex-anchored stubs must leave exactly that one candidate.
    // Before #709 every regex stub was `always_bits`, so all N were candidates for every request
    // and Stage 2 ran N regex executions. This is the headline the `regex_anchored` bench measures.
    #[test]
    fn regex_dimension_collapses_candidates() {
        let preds: Vec<Value> = (0..100)
            .map(|i| json!([{"matches": {"path": format!("^/api/endpoint{i}$")}}]))
            .collect();
        let snap = snapshot(&preds);

        for target in [0usize, 42, 99] {
            let c = snap.candidates("GET", &format!("/api/endpoint{target}"));
            assert_eq!(
                c.count(),
                1,
                "exactly one regex stub survives for /api/endpoint{target}"
            );
            assert!(
                c.contains(target),
                "the {target}-anchored stub is the survivor"
            );
        }
        // A path no pattern matches prunes every stub.
        assert_eq!(snap.candidates("GET", "/nothing").count(), 0);
    }

    // #731: `MultiRegex::mark` reuses a thread-local `PatternSet` (grow-then-clear). Verify the
    // reuse is correct when (a) automata of different `pattern_len` run on the same thread — a
    // larger capacity must not leak stale pids into a smaller search — and (b) both regex case
    // classes run within one request's `select`, i.e. `mark` is called twice in a row.
    #[test]
    fn regex_scratch_reuse_is_correct() {
        // (a) Large automaton first — grows the thread-local scratch capacity to 100.
        let big: Vec<Value> = (0..100)
            .map(|i| json!([{"matches": {"path": format!("^/e{i}$")}}]))
            .collect();
        let big_snap = snapshot(&big);
        assert_eq!(candidate_ids(&big_snap, "GET", "/e42"), vec![42]);

        // (b) Then a small automaton on the same thread — reuses the larger scratch via `clear`;
        // `iter` must yield only this search's pids, none left over from the 100-pattern search.
        let small = snapshot(&[
            json!([{"matches": {"path": "^/only$"}}]),
            json!([{"matches": {"path": "^/other$"}}]),
        ]);
        assert_eq!(candidate_ids(&small, "GET", "/only"), vec![0]);
        assert!(candidate_ids(&small, "GET", "/nomatch").is_empty());

        // (c) Two case classes in one request → `mark` runs twice sequentially on this thread.
        let mixed = snapshot(&[
            json!([{"matches": {"path": "^/mix$"}}]), // 0 default (case-insensitive) class
            json!([{"matches": {"path": "^/MIX$"}, "caseSensitive": true}]), // 1 case-sensitive class
        ]);
        assert_eq!(candidate_ids(&mixed, "GET", "/mix"), vec![0]);
        assert_eq!(candidate_ids(&mixed, "GET", "/MIX"), vec![0, 1]);

        // (d) Back to the large automaton on the same thread — scratch already at capacity 100.
        assert_eq!(candidate_ids(&big_snap, "GET", "/e7"), vec![7]);
    }

    // #731: `PathDimension::select` probes the map with the borrowed `&str` when the request path
    // has no uppercase ASCII byte (no allocation) and folds only when one is present. Both branches
    // must resolve identically to the previous fold-always behavior.
    #[test]
    fn path_dimension_fast_path_matches_regardless_of_case() {
        let snap = snapshot(&[
            json!([{"equals": {"path": "/users"}}]), // 0 anchor folds to "/users"
            json!([{"equals": {"path": "/Admin"}}]), // 1 anchor folds to "/admin"
        ]);
        // Already-lowercase request → borrowed-&str fast path, matches stub 0.
        assert_eq!(candidate_ids(&snap, "GET", "/users"), vec![0]);
        // Uppercase in the request → folded, matches stub 1's folded anchor.
        assert_eq!(candidate_ids(&snap, "GET", "/ADMIN"), vec![1]);
        // Lowercase request equal to the folded anchor also matches via the fast path.
        assert_eq!(candidate_ids(&snap, "GET", "/admin"), vec![1]);
        assert!(candidate_ids(&snap, "GET", "/nope").is_empty());
    }

    // The regex dimension must report EVERY pattern that matches a path, not just the
    // leftmost-first winner. Two patterns in one case class that can both match the same path is
    // the ONLY shape that distinguishes `MatchKind::All` from the meta engine's default
    // (`LeftmostFirst`) — under the default, `which_overlapping_matches` reports one of them and
    // the other stub is silently pruned even though its regex matches. Regression test for exactly
    // that under-approximation, which every mutually-exclusive-pattern test above cannot see.
    #[test]
    fn overlapping_patterns_are_all_candidates() {
        let snap = snapshot(&[
            json!([{"matches": {"path": "^/api"}}]),      // 0 broad
            json!([{"matches": {"path": "^/api/v1$"}}]),  // 1 specific — both match /api/v1
            json!([{"matches": {"path": "[0-9]+"}}]),     // 2 unanchored — matches /a0
            json!([{"matches": {"path": "^/a[0-9]*$"}}]), // 3 anchored — also matches /a0
        ]);
        let c = snap.candidates("GET", "/api/v1");
        assert!(
            c.contains(0) && c.contains(1),
            "both overlapping /api patterns must survive (earlier masking later)"
        );
        let c = snap.candidates("GET", "/a0");
        assert!(
            c.contains(2) && c.contains(3),
            "both overlapping /a0 patterns must survive (later masking earlier)"
        );

        // ...and the end-to-end consequence: a stub whose pattern matches must stay reachable even
        // when an earlier stub's pattern also matches the same path but the earlier stub is
        // rejected by Stage 2 on another field.
        let imp = imposter(&[
            json!([{"matches": {"path": "^/api"}}, {"equals": {"method": "POST"}}]),
            json!([{"matches": {"path": "^/api/v1$"}}]),
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        assert_eq!(
            idx(imp.find_matching_stub_with_client(
                "GET",
                "/api/v1",
                &no_headers,
                None,
                None,
                None,
                None
            )),
            Some(1),
            "the GET must fall through the POST-gated stub to the stub that matches"
        );
    }

    // AC4 (the load-bearing semantics test): `matches` does NOT fold like the other operators.
    // `fields.rs:227` builds the regex as `cached_regex(pattern, !case_sensitive)`, i.e. the
    // default is `RegexBuilder::case_insensitive(true)` — the regex crate's *Unicode* fold, not
    // the ASCII `eq_ignore_ascii_case` the path dimension's `fold()` mirrors. The dimension must
    // reproduce the evaluator's fold exactly, for case-sensitive, case-insensitive, and
    // inline-flag patterns alike — so this asserts against the linear oracle, not against a
    // hand-written expectation.
    #[test]
    fn regex_dimension_case_semantics_match_the_evaluator() {
        let imp = imposter(&[
            json!([{"matches": {"path": "^/Case$"}}]), // 0 default => Unicode case-INsensitive
            json!([{"matches": {"path": "^/Case2$"}, "caseSensitive": true}]), // 1 case-sensitive
            json!([{"matches": {"path": "(?i)^/inline$"}, "caseSensitive": true}]), // 2 inline (?i) overrides CS
            json!([{"matches": {"path": "(?-i)^/noinline$"}}]), // 3 inline (?-i) overrides the CI default
            json!([{"matches": {"path": "^/ünï$"}}]), // 4 non-ASCII under the Unicode fold
            json!([{"matches": {"path": "^/STRASSE$"}}]), // 5 non-ASCII fold edge
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [
            ("GET", "/case"),
            ("GET", "/Case"),
            ("GET", "/CASE"),
            ("GET", "/case2"),
            ("GET", "/Case2"),
            ("GET", "/inline"),
            ("GET", "/INLINE"),
            ("GET", "/noinline"),
            ("GET", "/NOINLINE"),
            ("GET", "/ünï"),
            ("GET", "/ÜNÏ"),
            ("GET", "/strasse"),
            ("GET", "/STRASSE"),
            ("GET", "/straße"),
        ] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }
    }

    // AC2 + the soundness rule: every regex shape the dimension cannot index must stay a candidate
    // for EVERY request. `except` rewrites the compared value and `selector` re-scopes it, so
    // neither can be matched against the raw path; a non-string pattern is still a real constraint
    // (the evaluator renders it via `to_string`); and or/not/and nesting is not a *required*
    // top-level constraint. Under-approximating any of these is a silent no-match.
    #[test]
    fn ineligible_regex_shapes_are_always_candidates() {
        let snap = snapshot(&[
            json!([{"matches": {"path": "^/idx$"}}]), // 0 indexable
            json!([{"matches": {"path": "^/x$"}, "except": "Y"}]), // 1 except rewrites the value
            json!([{"matches": {"path": "^/x$"}, "jsonpath": {"selector": "$.id"}}]), // 2 selector
            json!([{"or": [{"matches": {"path": "^/x$"}}, {"equals": {"path": "/y"}}]}]), // 3 or
            json!([{"not": {"matches": {"path": "^/x$"}}}]), // 4 not
            json!([{"and": [{"matches": {"path": "^/x$"}}, {"equals": {"method": "GET"}}]}]), // 5 and
            json!([{"matches": {"body": "^ping$"}}]), // 6 non-path field
            json!([{"matches": {"path": 7}}]),        // 7 non-string pattern is still a constraint
            json!([{"matches": {"path": "^/unclosed["}}]), // 8 invalid pattern never compiles
        ]);
        // A path matching no indexable pattern: only stubs the dimension cannot index may survive.
        let c = snap.candidates("GET", "/other");
        assert!(!c.contains(0), "the indexable /idx stub must be pruned");
        for i in [1, 2, 3, 4, 5, 6, 7, 8] {
            assert!(
                c.contains(i),
                "stub {i} is un-indexable → must always be a candidate"
            );
        }
    }

    // AC2: an invalid pattern must not take the whole automaton down with it. `new_many` fails on
    // the first bad pattern in the set, so a single typo'd stub would otherwise cost every other
    // regex stub its pruning. The bad stub lands in always_bits (sound: the evaluator's
    // `build_regex` returns None → the predicate is false → it never matches anyway) while its
    // well-formed neighbours keep being indexed.
    #[test]
    fn invalid_pattern_does_not_disable_the_dimension() {
        let imp = imposter(&[
            json!([{"matches": {"path": "^/good1$"}}]),
            json!([{"matches": {"path": "^/unclosed["}}]), // never compiles
            json!([{"matches": {"path": "^/good2$"}}]),
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [
            ("GET", "/good1"),
            ("GET", "/good2"),
            ("GET", "/unclosed["),
            ("GET", "/other"),
        ] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }

        // The valid neighbours are still pruned by the dimension — the bad pattern only costs
        // itself.
        let snap = snapshot(&[
            json!([{"matches": {"path": "^/good1$"}}]),
            json!([{"matches": {"path": "^/unclosed["}}]),
            json!([{"matches": {"path": "^/good2$"}}]),
        ]);
        let c = snap.candidates("GET", "/good1");
        assert!(c.contains(0), "the matching stub survives");
        assert!(
            !c.contains(2),
            "the non-matching valid stub is still pruned"
        );
        assert!(c.contains(1), "the invalid-pattern stub stays a candidate");
    }

    // AC2: a pattern the many-build rejects on size lands its stub in always_bits and leaves
    // behaviour unchanged — the index degrades to the pre-#709 fallback for that stub rather than
    // pruning it or failing the build.
    #[test]
    fn oversized_regex_set_falls_back_to_always_bits() {
        // A bounded repeat of a large Unicode class explodes the NFA well past NFA_SIZE_LIMIT.
        let huge = format!("^/{}$", r"\p{L}{5000}".repeat(4));
        let imp = imposter(&[
            json!([{"matches": {"path": huge}}]),
            json!([{"matches": {"path": "^/small$"}}]),
        ]);
        let no_headers: HashMap<String, String> = HashMap::new();
        for (m, p) in [("GET", "/small"), ("GET", "/abc"), ("GET", "/other")] {
            let linear =
                idx(imp.find_matching_stub_linear(m, p, &no_headers, None, None, None, None));
            let indexed =
                idx(imp.find_matching_stub_with_client(m, p, &no_headers, None, None, None, None));
            assert_eq!(indexed, linear, "index diverged from linear for {m} {p}");
        }
        // The oversized stub is never pruned; its well-formed neighbour still is.
        let snap = snapshot(&[
            json!([{"matches": {"path": huge}}]),
            json!([{"matches": {"path": "^/small$"}}]),
        ]);
        let c = snap.candidates("GET", "/nothing-matches");
        assert!(
            c.contains(0),
            "the oversized-pattern stub must stay a candidate"
        );
        assert!(!c.contains(1), "the small-pattern stub is still pruned");
    }

    // AC2: the fallback is *logged* at build, naming the stub and the offending pattern. Without
    // this an operator whose stub quietly stopped being indexed (a latency regression, not a
    // behaviour change) has nothing to correlate against.
    #[traced_test]
    #[test]
    fn rejected_pattern_warns_at_build_naming_the_stub() {
        let huge = format!("^/{}$", r"\p{L}{5000}".repeat(4));
        let _snap = snapshot(&[
            json!([{"matches": {"path": "^/small$"}}]),
            json!([{"matches": {"path": huge}}]),
        ]);
        assert!(
            logs_contain("regex dimension: pattern rejected by the multi-pattern build"),
            "the rejected pattern must be warned about at build time"
        );
        assert!(
            logs_contain("stub=1"),
            "the warn must name which stub lost its index"
        );
    }

    // Issue #475: the has_scenario_gate flag — computed once at index build — detects a
    // `requiredScenarioState` stub so the bounded matcher offloads the gate's flow-store read to
    // spawn_blocking on a blocking backend, while a scenario-free set keeps the inline fast path.
    #[test]
    fn has_scenario_gate_detects_required_scenario_state() {
        let build = |v: Value| {
            let states: Vec<Arc<StubState>> = v
                .as_array()
                .expect("array")
                .iter()
                .map(|s| {
                    Arc::new(StubState::new(
                        serde_json::from_value(s.clone()).expect("stub"),
                    ))
                })
                .collect();
            StubSnapshot::build(states)
        };
        let ungated = build(json!([
            { "predicates": [{"equals": {"path": "/a"}}], "responses": [{"is": {"statusCode": 200}}] }
        ]));
        assert!(!ungated.has_scenario_gate());

        let gated = build(json!([
            {
                "predicates": [{"equals": {"path": "/a"}}],
                "scenarioName": "sc",
                "requiredScenarioState": "started",
                "responses": [{"is": {"statusCode": 200}}]
            }
        ]));
        assert!(gated.has_scenario_gate());
    }

    // Issue #476: the has_inject gate — computed once at index build — detects an inject
    // predicate anywhere in a stub's predicate tree, including nested under and/or/not, and
    // stays false for scriptless stub sets so they keep the inline matching fast path.
    #[test]
    fn has_inject_detects_top_level_and_nested() {
        let scriptless = StubSnapshot::build(stub_states(&[
            json!([{"equals": {"path": "/a"}}]),
            json!([{"and": [{"equals": {"path": "/b"}}, {"exists": {"query": {"q": true}}}]}]),
        ]));
        assert!(!scriptless.has_inject());

        let top_level = StubSnapshot::build(stub_states(&[
            json!([{"equals": {"path": "/a"}}]),
            json!([{"inject": "function (config) { return true; }"}]),
        ]));
        assert!(top_level.has_inject());

        let under_and = StubSnapshot::build(stub_states(&[json!([
            {"and": [{"equals": {"path": "/a"}}, {"inject": "function (config) { return true; }"}]}
        ])]));
        assert!(under_and.has_inject());

        let under_not_in_or = StubSnapshot::build(stub_states(&[json!([
            {"or": [{"equals": {"path": "/a"}}, {"not": {"inject": "function (config) { return true; }"}}]}
        ])]));
        assert!(under_not_in_or.has_inject());
    }

    // ---- issue #708: deepEquals body-hash dimension ----

    fn deep_body(expected: Value) -> Value {
        json!([{"deepEquals": {"body": expected}}])
    }

    #[test]
    fn body_dimension_collapses_candidates() {
        // 500 deepEquals-body stubs, each expecting a distinct object. A request equal to one must
        // probe down to just that stub — O(stubs) structural comparisons collapse to one hash probe.
        let preds: Vec<Value> = (0..500)
            .map(|i| deep_body(json!({"id": i, "kind": "order"})))
            .collect();
        let snap = snapshot(&preds);

        let target = 437usize;
        let ids = candidate_ids_body(&snap, "POST", "/x", &json!({"id": target, "kind": "order"}));
        assert_eq!(ids.len(), 1, "collapses to a single candidate, got {ids:?}");
        assert_eq!(ids, vec![target]);

        // A body equal to none of them prunes the whole dimension to empty.
        assert!(
            candidate_ids_body(&snap, "POST", "/x", &json!({"id": 9999, "kind": "order"}))
                .is_empty()
        );
    }

    #[test]
    fn body_dimension_matches_across_coercion_key_order_and_case() {
        let snap = snapshot(&[
            deep_body(json!({"a": 1, "b": "x"})), // 0
            deep_body(json!({"a": 2})),           // 1
        ]);
        // number↔string coercion, object key order, and ASCII key/value case all fold to stub 0.
        let ids = candidate_ids_body(&snap, "POST", "/p", &json!({"B": "X", "a": "1"}));
        assert_eq!(ids, vec![0]);
    }

    #[test]
    fn body_dimension_ineligible_stubs_are_never_pruned() {
        let snap = snapshot(&[
            json!([{"deepEquals": {"body": {"a": 1}}, "caseSensitive": true}]), // 0 caseSensitive
            json!([{"deepEquals": {"body": {"a": 1}}, "keyCaseSensitive": true}]), // 1 keyCaseSensitive
            json!([{"deepEquals": {"body": {"a": 1}}, "jsonpath": {"selector": "$.x"}}]), // 2 selector
            json!([{"deepEquals": {"body": "hello"}}]), // 3 scalar body (raw-string compare)
            json!([{"deepEquals": {"body": {"a": "{\"k\":1}"}}}]), // 4 JSON-in-string leaf in expected
            json!([{"deepEquals": {"body": {"a": 1}}, "except": "x"}]), // 5 except (rewrites the value)
            deep_body(json!({"eligible": true})),                       // 6 ELIGIBLE (indexed)
        ]);
        // A body matching only stub 6: the eligible stub is hit by the probe; the six ineligible
        // stubs are `always` and must all remain — none can be hash-pruned.
        let hit = candidate_ids_body(&snap, "POST", "/p", &json!({"eligible": true}));
        assert_eq!(hit, vec![0, 1, 2, 3, 4, 5, 6]);
        // A body matching none: only the eligible stub 6 is pruned; the carve-outs stay candidates.
        let miss = candidate_ids_body(&snap, "POST", "/p", &json!({"zzz": 0}));
        assert_eq!(miss, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn body_dimension_non_json_body_excludes_indexed_stubs() {
        let snap = snapshot(&[
            deep_body(json!({"a": 1})),          // 0 indexed body stub
            json!([{"equals": {"path": "/p"}}]), // 1 path stub
        ]);
        // `candidate_ids` sends no body (the non-JSON/absent case): the indexed body stub cannot
        // match, so it is excluded; the path stub still matches /p.
        assert_eq!(candidate_ids(&snap, "GET", "/p"), vec![1]);
    }

    #[test]
    fn body_dimension_json_in_string_request_widens() {
        let snap = snapshot(&[
            deep_body(json!({"a": {"b": 1}})), // 0 expects a nested object
            deep_body(json!({"z": 9})),        // 1
        ]);
        // The request body carries a JSON-ish string leaf, equivalent (both directions) to the
        // object stub 0 expects. The dimension must not prune — it widens to every candidate so
        // Stage-2 can confirm stub 0. (Soundness over precision: stub 1 rides along.)
        let ids = candidate_ids_body(&snap, "POST", "/p", &json!({"a": "{\"b\":1}"}));
        assert_eq!(
            ids,
            vec![0, 1],
            "a JSON-ish string leaf disables pruning for the dimension"
        );
    }
}
