//! High-performance rule indexing for fast request matching.
//!
//! This module provides optimized data structures for routing requests to rules:
//!
//! - **Radix Trie** (`matchit`): O(k) path lookup for exact and parameterized routes
//! - **Aho-Corasick**: O(n) multi-pattern matching for `contains` predicates
//! - **Hash Map**: O(1) exact path and method lookups
//!
//! # Architecture
//!
//! ```text
//! Request → Stage 1 (Fast Index) → Candidate Rules → Stage 2 (Full Predicate) → Match
//! ```
//!
//! Stage 1 uses optimized data structures to quickly narrow down candidate rules.
//! Stage 2 evaluates full predicates only on candidates, not all rules.

// Allow dead code while rule index is being integrated
#![allow(dead_code)]

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use matchit::Router;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::predicate::{CompiledRequestPredicate, PathMatcher, RequestPredicate};

/// Index entry storing rule ID and compiled predicate.
#[derive(Debug, Clone)]
pub struct IndexedRule {
    /// Unique rule identifier
    pub id: usize,
    /// Rule name for debugging
    pub name: String,
    /// Compiled predicate for full evaluation
    pub predicate: CompiledRequestPredicate,
    /// Priority (lower = higher priority)
    pub priority: u32,
}

/// Categorization of path matchers for indexing strategy.
#[derive(Debug, Clone, PartialEq)]
enum PathCategory {
    /// Matches any path - must check all requests
    Any,
    /// Exact path match - can use HashMap
    Exact(String),
    /// Prefix match - can use Radix Trie
    Prefix(String),
    /// Contains substring - can use Aho-Corasick
    Contains(String),
    /// Ends with suffix - needs linear scan but can optimize
    EndsWith(String),
    /// Regex pattern - needs linear scan
    Regex,
}

/// High-performance rule index using multiple data structures.
///
/// Provides O(1) to O(k) lookups for most common cases while falling back
/// to linear scan only for complex patterns.
#[derive(Debug)]
pub struct RuleIndex {
    /// All indexed rules
    rules: Vec<IndexedRule>,

    /// Exact path → rule IDs (O(1) lookup)
    exact_paths: HashMap<String, Vec<usize>>,

    /// Radix trie for prefix matching (O(k) lookup)
    prefix_router: Router<Vec<usize>>,
    /// Count of prefix routes (Router doesn't expose len())
    prefix_routes_count: usize,

    /// Aho-Corasick automaton for contains matching (O(n) multi-pattern)
    contains_automaton: Option<AhoCorasick>,
    /// Pattern index → rule IDs for contains matches
    contains_pattern_rules: Vec<Vec<usize>>,

    /// EndsWith patterns (checked linearly but typically few)
    ends_with_patterns: Vec<(String, Vec<usize>)>,

    /// Rules with regex patterns (linear scan required)
    regex_rules: Vec<usize>,

    /// Rules that match any path (always checked)
    any_path_rules: Vec<usize>,

    /// Method → rule IDs for quick method filtering
    method_index: HashMap<String, HashSet<usize>>,
}

impl RuleIndex {
    /// Create a new rule index from predicates.
    pub fn new() -> Self {
        RuleIndex {
            rules: Vec::new(),
            exact_paths: HashMap::new(),
            prefix_router: Router::new(),
            prefix_routes_count: 0,
            contains_automaton: None,
            contains_pattern_rules: Vec::new(),
            ends_with_patterns: Vec::new(),
            regex_rules: Vec::new(),
            any_path_rules: Vec::new(),
            method_index: HashMap::new(),
        }
    }

    /// Build an index from a list of request predicates.
    pub fn build(
        predicates: Vec<(String, RequestPredicate, u32)>,
    ) -> Result<Self, IndexBuildError> {
        let mut index = Self::new();
        let mut contains_patterns: Vec<String> = Vec::new();
        let mut contains_pattern_to_rules: HashMap<String, Vec<usize>> = HashMap::new();
        let mut prefix_routes: HashMap<String, Vec<usize>> = HashMap::new();

        for (idx, (name, predicate, priority)) in predicates.into_iter().enumerate() {
            // Compile the predicate
            let compiled = CompiledRequestPredicate::compile(&predicate)
                .map_err(|e| IndexBuildError::PredicateCompile(name.clone(), e.to_string()))?;

            // Categorize the path matcher
            let category = Self::categorize_path(&predicate.path);

            // Index by path category
            match &category {
                PathCategory::Any => {
                    index.any_path_rules.push(idx);
                }
                PathCategory::Exact(path) => {
                    index.exact_paths.entry(path.clone()).or_default().push(idx);
                }
                PathCategory::Prefix(prefix) => {
                    prefix_routes.entry(prefix.clone()).or_default().push(idx);
                }
                PathCategory::Contains(pattern) => {
                    if !contains_pattern_to_rules.contains_key(pattern) {
                        contains_patterns.push(pattern.clone());
                    }
                    contains_pattern_to_rules
                        .entry(pattern.clone())
                        .or_default()
                        .push(idx);
                }
                PathCategory::EndsWith(suffix) => {
                    // Find or create entry for this suffix
                    if let Some(entry) = index
                        .ends_with_patterns
                        .iter_mut()
                        .find(|(s, _)| s == suffix)
                    {
                        entry.1.push(idx);
                    } else {
                        index.ends_with_patterns.push((suffix.clone(), vec![idx]));
                    }
                }
                PathCategory::Regex => {
                    index.regex_rules.push(idx);
                }
            }

            // Index by method if specified
            if let Some(crate::predicate::StringMatcher::Equals(method)) = &predicate.method {
                index
                    .method_index
                    .entry(method.to_uppercase())
                    .or_default()
                    .insert(idx);
            }

            // Store the rule
            index.rules.push(IndexedRule {
                id: idx,
                name,
                predicate: compiled,
                priority,
            });
        }

        // Build Aho-Corasick automaton for contains patterns
        if !contains_patterns.is_empty() {
            let ac = AhoCorasickBuilder::new()
                .match_kind(MatchKind::LeftmostFirst)
                .build(&contains_patterns)
                .map_err(|e| IndexBuildError::AhoCorasick(e.to_string()))?;

            index.contains_automaton = Some(ac);

            // Map pattern indices to rule IDs
            for pattern in &contains_patterns {
                index.contains_pattern_rules.push(
                    contains_pattern_to_rules
                        .remove(pattern)
                        .unwrap_or_default(),
                );
            }
        }

        // Build radix trie for prefix routes
        for (prefix, rule_ids) in prefix_routes {
            // matchit requires a trailing wildcard for prefix matching
            let route = if prefix.ends_with('/') {
                format!("{prefix}{{*rest}}")
            } else {
                format!("{prefix}/{{*rest}}")
            };

            // Also add exact prefix match
            index
                .prefix_router
                .insert(&prefix, rule_ids.clone())
                .map_err(|e| IndexBuildError::RadixTrie(e.to_string()))?;

            index.prefix_routes_count += 1;

            // Add with wildcard for sub-paths
            if index.prefix_router.insert(&route, rule_ids).is_err() {
                // Ignore duplicate route errors - the exact match is sufficient
            }
        }

        Ok(index)
    }

    /// Categorize a path matcher for indexing strategy selection.
    fn categorize_path(path: &Option<PathMatcher>) -> PathCategory {
        match path {
            None => PathCategory::Any,
            Some(PathMatcher::Any) => PathCategory::Any,
            Some(PathMatcher::Exact { exact }) => PathCategory::Exact(exact.clone()),
            Some(PathMatcher::Prefix { prefix }) => PathCategory::Prefix(prefix.clone()),
            Some(PathMatcher::Contains { contains }) => PathCategory::Contains(contains.clone()),
            Some(PathMatcher::EndsWith { ends_with }) => PathCategory::EndsWith(ends_with.clone()),
            Some(PathMatcher::Regex { .. }) => PathCategory::Regex,
            Some(PathMatcher::Full { matcher, .. }) => {
                use crate::predicate::StringMatcher;
                match matcher {
                    StringMatcher::Equals(v) => PathCategory::Exact(v.clone()),
                    StringMatcher::StartsWith(v) => PathCategory::Prefix(v.clone()),
                    StringMatcher::Contains(v) => PathCategory::Contains(v.clone()),
                    StringMatcher::EndsWith(v) => PathCategory::EndsWith(v.clone()),
                    StringMatcher::Matches(_) => PathCategory::Regex,
                    StringMatcher::Exists(_) => PathCategory::Any,
                }
            }
        }
    }

    /// Find candidate rules for a request path (Stage 1).
    ///
    /// Returns rule IDs that might match based on fast index lookups.
    /// These candidates still need full predicate evaluation in Stage 2.
    pub fn find_candidates(&self, path: &str, method: Option<&str>) -> Vec<usize> {
        let mut candidates = HashSet::new();

        // 1. Always include "any path" rules
        for &id in &self.any_path_rules {
            candidates.insert(id);
        }

        // 2. Check exact path match (O(1))
        if let Some(ids) = self.exact_paths.get(path) {
            for &id in ids {
                candidates.insert(id);
            }
        }

        // 3. Check prefix matches via radix trie (O(k))
        if let Ok(matched) = self.prefix_router.at(path) {
            for &id in matched.value {
                candidates.insert(id);
            }
        }

        // 4. Check contains patterns via Aho-Corasick (O(n))
        if let Some(ref ac) = self.contains_automaton {
            for mat in ac.find_iter(path) {
                let pattern_idx = mat.pattern().as_usize();
                if let Some(ids) = self.contains_pattern_rules.get(pattern_idx) {
                    for &id in ids {
                        candidates.insert(id);
                    }
                }
            }
        }

        // 5. Check endsWith patterns (linear but typically few)
        for (suffix, ids) in &self.ends_with_patterns {
            if path.ends_with(suffix) {
                for &id in ids {
                    candidates.insert(id);
                }
            }
        }

        // 6. Include regex rules (must always be checked)
        for &id in &self.regex_rules {
            candidates.insert(id);
        }

        // 7. Filter by method if specified
        let mut result: Vec<usize> = if let Some(m) = method {
            let method_upper = m.to_uppercase();
            if let Some(method_rules) = self.method_index.get(&method_upper) {
                // Include rules that match this method OR have no method constraint
                candidates
                    .into_iter()
                    .filter(|id| method_rules.contains(id) || !self.has_method_constraint(*id))
                    .collect()
            } else {
                // No rules specifically for this method, include rules without method constraint
                candidates
                    .into_iter()
                    .filter(|id| !self.has_method_constraint(*id))
                    .collect()
            }
        } else {
            candidates.into_iter().collect()
        };

        // Sort by priority (lower = higher priority)
        result.sort_by_key(|&id| self.rules.get(id).map(|r| r.priority).unwrap_or(u32::MAX));

        result
    }

    /// Check if a rule has a method constraint.
    fn has_method_constraint(&self, rule_id: usize) -> bool {
        self.method_index
            .values()
            .any(|rules| rules.contains(&rule_id))
    }

    /// Get a rule by ID.
    pub fn get_rule(&self, id: usize) -> Option<&IndexedRule> {
        self.rules.get(id)
    }

    /// Get all rules.
    pub fn rules(&self) -> &[IndexedRule] {
        &self.rules
    }

    /// Get the number of indexed rules.
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Check if the index is empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Get index statistics for debugging/monitoring.
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            total_rules: self.rules.len(),
            exact_paths: self.exact_paths.len(),
            prefix_routes: self.prefix_routes_count,
            contains_patterns: self.contains_pattern_rules.len(),
            ends_with_patterns: self.ends_with_patterns.len(),
            regex_rules: self.regex_rules.len(),
            any_path_rules: self.any_path_rules.len(),
        }
    }
}

impl Default for RuleIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about the rule index.
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_rules: usize,
    pub exact_paths: usize,
    pub prefix_routes: usize,
    pub contains_patterns: usize,
    pub ends_with_patterns: usize,
    pub regex_rules: usize,
    pub any_path_rules: usize,
}

/// Errors that can occur during index building.
#[derive(Debug, Clone)]
pub enum IndexBuildError {
    PredicateCompile(String, String),
    AhoCorasick(String),
    RadixTrie(String),
}

impl std::fmt::Display for IndexBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexBuildError::PredicateCompile(name, err) => {
                write!(f, "Failed to compile predicate '{name}': {err}")
            }
            IndexBuildError::AhoCorasick(err) => {
                write!(f, "Failed to build Aho-Corasick automaton: {err}")
            }
            IndexBuildError::RadixTrie(err) => {
                write!(f, "Failed to build radix trie: {err}")
            }
        }
    }
}

impl std::error::Error for IndexBuildError {}

/// Thread-safe rule index wrapper for concurrent access.
#[derive(Debug, Clone)]
pub struct SharedRuleIndex {
    inner: Arc<RuleIndex>,
}

impl SharedRuleIndex {
    /// Create a new shared index.
    pub fn new(index: RuleIndex) -> Self {
        SharedRuleIndex {
            inner: Arc::new(index),
        }
    }

    /// Find candidate rules for a request.
    pub fn find_candidates(&self, path: &str, method: Option<&str>) -> Vec<usize> {
        self.inner.find_candidates(path, method)
    }

    /// Get a rule by ID.
    pub fn get_rule(&self, id: usize) -> Option<&IndexedRule> {
        self.inner.get_rule(id)
    }

    /// Get index statistics.
    pub fn stats(&self) -> IndexStats {
        self.inner.stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predicate::{PredicateOptions, StringMatcher};

    fn make_predicate(path: Option<PathMatcher>, method: Option<&str>) -> RequestPredicate {
        RequestPredicate {
            method: method.map(|m| StringMatcher::Equals(m.to_string())),
            path,
            headers: vec![],
            query: vec![],
            body: None,
            options: PredicateOptions::default(),
        }
    }

    #[test]
    fn test_exact_path_indexing() {
        let predicates = vec![
            (
                "rule1".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/api/users".to_string(),
                    }),
                    Some("GET"),
                ),
                0,
            ),
            (
                "rule2".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/api/items".to_string(),
                    }),
                    Some("GET"),
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        // Exact match should find rule
        let candidates = index.find_candidates("/api/users", Some("GET"));
        assert!(candidates.contains(&0));
        assert!(!candidates.contains(&1));

        // Different path should not match
        let candidates = index.find_candidates("/api/other", Some("GET"));
        assert!(!candidates.contains(&0));
        assert!(!candidates.contains(&1));
    }

    #[test]
    fn test_prefix_path_indexing() {
        let predicates = vec![
            (
                "api_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::Prefix {
                        prefix: "/api".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "admin_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::Prefix {
                        prefix: "/admin".to_string(),
                    }),
                    None,
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        // Prefix match
        let candidates = index.find_candidates("/api/users", None);
        assert!(candidates.contains(&0));
        assert!(!candidates.contains(&1));

        // Different prefix
        let candidates = index.find_candidates("/admin/dashboard", None);
        assert!(!candidates.contains(&0));
        assert!(candidates.contains(&1));
    }

    #[test]
    fn test_contains_path_indexing() {
        let predicates = vec![
            (
                "users_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::Contains {
                        contains: "users".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "items_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::Contains {
                        contains: "items".to_string(),
                    }),
                    None,
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        // Contains match
        let candidates = index.find_candidates("/api/users/123", None);
        assert!(candidates.contains(&0));
        assert!(!candidates.contains(&1));

        // Contains in different position
        let candidates = index.find_candidates("/v2/items/list", None);
        assert!(!candidates.contains(&0));
        assert!(candidates.contains(&1));
    }

    #[test]
    fn test_ends_with_indexing() {
        let predicates = vec![
            (
                "json_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::EndsWith {
                        ends_with: ".json".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "xml_rule".to_string(),
                make_predicate(
                    Some(PathMatcher::EndsWith {
                        ends_with: ".xml".to_string(),
                    }),
                    None,
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        let candidates = index.find_candidates("/data/export.json", None);
        assert!(candidates.contains(&0));
        assert!(!candidates.contains(&1));

        let candidates = index.find_candidates("/data/export.xml", None);
        assert!(!candidates.contains(&0));
        assert!(candidates.contains(&1));
    }

    #[test]
    fn test_any_path_always_included() {
        let predicates = vec![
            (
                "catch_all".to_string(),
                make_predicate(None, None),
                10, // Lower priority
            ),
            (
                "specific".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/api".to_string(),
                    }),
                    None,
                ),
                0, // Higher priority
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        // Any path rule should always be in candidates
        let candidates = index.find_candidates("/random/path", None);
        assert!(candidates.contains(&0)); // catch_all

        // Both should match for /api
        let candidates = index.find_candidates("/api", None);
        assert!(candidates.contains(&0));
        assert!(candidates.contains(&1));
        // Higher priority (lower number) should come first
        assert_eq!(candidates[0], 1); // specific rule first
    }

    #[test]
    fn test_method_filtering() {
        let predicates = vec![
            (
                "get_users".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/users".to_string(),
                    }),
                    Some("GET"),
                ),
                0,
            ),
            (
                "post_users".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/users".to_string(),
                    }),
                    Some("POST"),
                ),
                0,
            ),
            (
                "any_method".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/users".to_string(),
                    }),
                    None,
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();

        // GET should match get_users and any_method
        let candidates = index.find_candidates("/users", Some("GET"));
        assert!(candidates.contains(&0)); // get_users
        assert!(!candidates.contains(&1)); // not post_users
        assert!(candidates.contains(&2)); // any_method

        // POST should match post_users and any_method
        let candidates = index.find_candidates("/users", Some("POST"));
        assert!(!candidates.contains(&0)); // not get_users
        assert!(candidates.contains(&1)); // post_users
        assert!(candidates.contains(&2)); // any_method
    }

    #[test]
    fn test_index_stats() {
        let predicates = vec![
            ("r1".to_string(), make_predicate(None, None), 0),
            (
                "r2".to_string(),
                make_predicate(
                    Some(PathMatcher::Exact {
                        exact: "/a".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "r3".to_string(),
                make_predicate(
                    Some(PathMatcher::Prefix {
                        prefix: "/b".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "r4".to_string(),
                make_predicate(
                    Some(PathMatcher::Contains {
                        contains: "c".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "r5".to_string(),
                make_predicate(
                    Some(PathMatcher::EndsWith {
                        ends_with: ".d".to_string(),
                    }),
                    None,
                ),
                0,
            ),
            (
                "r6".to_string(),
                make_predicate(
                    Some(PathMatcher::Regex {
                        regex: ".*".to_string(),
                    }),
                    None,
                ),
                0,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();
        let stats = index.stats();

        assert_eq!(stats.total_rules, 6);
        assert_eq!(stats.any_path_rules, 1);
        assert_eq!(stats.exact_paths, 1);
        assert_eq!(stats.prefix_routes, 1);
        assert_eq!(stats.contains_patterns, 1);
        assert_eq!(stats.ends_with_patterns, 1);
        assert_eq!(stats.regex_rules, 1);
    }

    #[test]
    fn test_priority_ordering() {
        let predicates = vec![
            ("low_priority".to_string(), make_predicate(None, None), 100),
            ("high_priority".to_string(), make_predicate(None, None), 1),
            (
                "medium_priority".to_string(),
                make_predicate(None, None),
                50,
            ),
        ];

        let index = RuleIndex::build(predicates).unwrap();
        let candidates = index.find_candidates("/any", None);

        // Should be sorted by priority (ascending)
        assert_eq!(candidates, vec![1, 2, 0]); // high, medium, low
    }
}
