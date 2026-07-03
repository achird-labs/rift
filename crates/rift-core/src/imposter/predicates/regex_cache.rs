//! Process-wide cache of compiled predicate regexes.
//!
//! `matches` predicates and `except` patterns previously recompiled a `regex::Regex`
//! from its source string on every request, per field — regex compilation is orders of
//! magnitude more expensive than matching, so this dominated the imposter matching hot
//! path. Compile each distinct `(pattern, case_insensitive)` once and reuse the cached
//! `Arc<Regex>`, mirroring the `Arc<Regex>` the proxy engine keeps in `predicate/matcher.rs`.
//!
//! Patterns originate from imposter configuration, not request data, so the working set is
//! small. The cache is nonetheless **capacity-bounded**: because it is a process-global
//! static, repeatedly creating and deleting imposters with ever-distinct patterns would
//! otherwise grow it for the life of the process. On reaching [`MAX_CACHED_REGEXES`] the map
//! is reset (dropping stale entries); overflow patterns simply recompile on their next miss.

use parking_lot::RwLock;
use regex::{Regex, RegexBuilder};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// Per-map ceiling on distinct cached regexes. Comfortably above any realistic imposter
/// config's pattern count; the bound only exists to cap growth under runtime imposter churn.
const MAX_CACHED_REGEXES: usize = 1024;

#[derive(Default)]
struct RegexCache {
    sensitive: HashMap<String, Arc<Regex>>,
    insensitive: HashMap<String, Arc<Regex>>,
}

static REGEX_CACHE: LazyLock<RwLock<RegexCache>> =
    LazyLock::new(|| RwLock::new(RegexCache::default()));

fn compile(pattern: &str, case_insensitive: bool) -> Result<Regex, regex::Error> {
    if case_insensitive {
        RegexBuilder::new(pattern).case_insensitive(true).build()
    } else {
        Regex::new(pattern)
    }
}

/// Return the compiled regex for `pattern`, compiling and caching it on first use.
///
/// `case_insensitive` is part of the key: the same source string compiled with and
/// without the case-insensitive flag are distinct, independently cached regexes.
/// Returns `None` when `pattern` fails to compile (callers treat this as "no match"),
/// preserving the previous per-request behavior of a failed `Regex::new`.
pub(crate) fn cached_regex(pattern: &str, case_insensitive: bool) -> Option<Arc<Regex>> {
    // Fast path: shared read lock, no allocation on a hit.
    {
        let cache = REGEX_CACHE.read();
        let map = if case_insensitive {
            &cache.insensitive
        } else {
            &cache.sensitive
        };
        if let Some(re) = map.get(pattern) {
            return Some(Arc::clone(re));
        }
    }

    // Slow path (cache miss): compile once (outside the lock), then insert under the write lock.
    let compiled = Arc::new(compile(pattern, case_insensitive).ok()?);
    let mut cache = REGEX_CACHE.write();
    let map = if case_insensitive {
        &mut cache.insensitive
    } else {
        &mut cache.sensitive
    };
    // Another thread may have inserted this pattern while we compiled — reuse its entry.
    if let Some(re) = map.get(pattern) {
        return Some(Arc::clone(re));
    }
    if map.len() >= MAX_CACHED_REGEXES {
        map.clear();
    }
    map.insert(pattern.to_string(), Arc::clone(&compiled));
    Some(compiled)
}

#[cfg(test)]
fn cached_len(case_insensitive: bool) -> usize {
    let cache = REGEX_CACHE.read();
    if case_insensitive {
        cache.insensitive.len()
    } else {
        cache.sensitive.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_regex_reuses_same_arc() {
        let a = cached_regex("cache-me-[0-9]+", false).expect("compiles");
        let b = cached_regex("cache-me-[0-9]+", false).expect("compiles");
        assert!(
            Arc::ptr_eq(&a, &b),
            "second lookup must return the cached Arc, not a freshly compiled regex"
        );
        assert!(a.is_match("cache-me-42"));
    }

    #[test]
    fn cached_regex_keys_on_case_sensitivity() {
        let sensitive = cached_regex("KEYCASE-ABC", false).expect("compiles");
        let insensitive = cached_regex("KEYCASE-ABC", true).expect("compiles");
        assert!(
            !Arc::ptr_eq(&sensitive, &insensitive),
            "case-sensitive and case-insensitive variants must cache separately"
        );
        assert!(sensitive.is_match("KEYCASE-ABC"));
        assert!(!sensitive.is_match("keycase-abc"));
        assert!(insensitive.is_match("keycase-abc"));
    }

    #[test]
    fn cached_regex_invalid_returns_none() {
        assert!(
            cached_regex("invalid-([0-9]+", false).is_none(),
            "an unparseable pattern must return None (callers treat as no match)"
        );
    }

    #[test]
    fn cache_stays_bounded_under_distinct_patterns() {
        // Insert well past the cap with distinct patterns; the map must never exceed the
        // ceiling (it resets on overflow), so runtime imposter churn can't grow it forever.
        for i in 0..(MAX_CACHED_REGEXES + 200) {
            let _ = cached_regex(&format!("bounded-probe-{i}-[a-z]+"), false);
        }
        assert!(
            cached_len(false) <= MAX_CACHED_REGEXES,
            "sensitive cache must stay within MAX_CACHED_REGEXES"
        );
    }
}
