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
//! evicts a **batch** of entries (roughly a quarter of the cap), not the whole map: a working
//! set that cycles just over the cap keeps most of its previously-compiled regexes hot instead
//! of hitting a full recompile storm every time it crosses the ceiling.
//!
//! Reads are lock-free: the backing store is [`papaya::HashMap`], which lets concurrent readers
//! (the matching hot path, per request) proceed without contending on a lock. Only a cache
//! *miss* pays for compilation and an insert; hits are a single atomic-guarded lookup.

use papaya::HashMap as PapayaMap;
use regex::{Regex, RegexBuilder};
use std::sync::{Arc, LazyLock};

/// Per-map ceiling on distinct cached regexes. Comfortably above any realistic imposter
/// config's pattern count; the bound only exists to cap growth under runtime imposter churn.
const MAX_CACHED_REGEXES: usize = 1024;

/// How many entries a single overflow evicts. A quarter of the cap leaves the majority of the
/// map intact, so a working set that cycles just over the cap doesn't get fully recompiled on
/// every overflow (the defect `clear()`-on-overflow had).
const EVICTION_BATCH: usize = MAX_CACHED_REGEXES / 4;

/// `papaya`'s own map-level synchronization already makes the store lock-free; each `String` key
/// is cheap to hash and the working set is operator/stub-derived (bounded, not attacker-supplied
/// per request), so the extra HashDoS resistance of `papaya`'s default hasher over `foldhash`
/// buys nothing here. `foldhash::fast::RandomState` matches the hasher already used for other
/// hot-path maps in `crate::util`.
type CacheMap = PapayaMap<String, Arc<Regex>, foldhash::fast::RandomState>;

fn new_cache_map() -> CacheMap {
    PapayaMap::with_hasher(foldhash::fast::RandomState::default())
}

/// The two case classes are kept as separate maps (rather than one map keyed on
/// `(pattern, case_insensitive)`) so each class's [`MAX_CACHED_REGEXES`] ceiling — and the batch
/// eviction it triggers — is independent: churn in one case class can't evict the other's
/// entries.
struct RegexCache {
    sensitive: CacheMap,
    insensitive: CacheMap,
}

impl Default for RegexCache {
    fn default() -> Self {
        Self {
            sensitive: new_cache_map(),
            insensitive: new_cache_map(),
        }
    }
}

static REGEX_CACHE: LazyLock<RegexCache> = LazyLock::new(RegexCache::default);

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
    let map = if case_insensitive {
        &REGEX_CACHE.insensitive
    } else {
        &REGEX_CACHE.sensitive
    };
    let guard = map.pin();

    // Fast path: a lock-free guarded read, no allocation on a hit.
    if let Some(re) = guard.get(pattern) {
        return Some(Arc::clone(re));
    }

    // Slow path (cache miss): compile outside any critical section — papaya never takes a lock,
    // but compilation is the expensive part and has no business happening more than once per
    // insert attempt.
    let compiled = Arc::new(compile(pattern, case_insensitive).ok()?);

    // Batch-evict on overflow instead of `clear()`-ing: drop a fixed fraction of entries so the
    // rest of the map (the majority of the working set) stays hot across the overflow.
    if guard.len() >= MAX_CACHED_REGEXES {
        let victims: Vec<String> = guard.keys().take(EVICTION_BATCH).cloned().collect();
        for victim in &victims {
            guard.remove(victim);
        }
    }

    // `get_or_insert_with` double-checks atomically: if another thread inserted this exact
    // pattern while we were compiling, we discard our copy and reuse theirs.
    Some(Arc::clone(
        guard.get_or_insert_with(pattern.to_string(), || compiled),
    ))
}

#[cfg(test)]
fn cached_len(case_insensitive: bool) -> usize {
    let map = if case_insensitive {
        &REGEX_CACHE.insensitive
    } else {
        &REGEX_CACHE.sensitive
    };
    map.pin().len()
}

/// True iff `(pattern, case_insensitive)` is currently cached. A pure probe: unlike
/// [`cached_regex`], it never compiles or inserts, so it can check survivorship after an eviction
/// without perturbing the cache under test.
#[cfg(test)]
fn is_cached(pattern: &str, case_insensitive: bool) -> bool {
    let map = if case_insensitive {
        &REGEX_CACHE.insensitive
    } else {
        &REGEX_CACHE.sensitive
    };
    map.pin().contains_key(pattern)
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
        // Insert well past the cap with distinct patterns; the map must stay near the ceiling
        // (overflow evicts a batch), so runtime imposter churn can't grow it forever. A small
        // slack over the cap is allowed for the batch-eviction boundary (single-threaded here).
        for i in 0..(MAX_CACHED_REGEXES + 200) {
            let _ = cached_regex(&format!("bounded-probe-{i}-[a-z]+"), false);
        }
        assert!(
            cached_len(false) <= MAX_CACHED_REGEXES,
            "sensitive cache must stay within MAX_CACHED_REGEXES (saw {})",
            cached_len(false)
        );
    }

    // AC (the eviction-cliff fix): overflow must NOT flush the whole cache. Fill to the cap, then
    // push past it, and assert a large fraction of the earlier entries survive — the old
    // clear-on-overflow would drop ALL of them, causing a recompile storm for a working set that
    // cycles just over the cap. With batch eviction (a bounded fraction dropped per overflow), most
    // entries remain hot.
    #[test]
    fn overflow_evicts_a_batch_not_the_whole_cache() {
        // Use the insensitive map so this test is independent of others sharing the sensitive one.
        // Fill to just under the cap.
        let fill = MAX_CACHED_REGEXES - 1;
        for i in 0..fill {
            let _ = cached_regex(&format!("cliff-fill-{i}-[a-z]+"), true).expect("compiles");
        }
        // Push ~30% past the cap to force at least one overflow eviction.
        let overshoot = MAX_CACHED_REGEXES / 3;
        for i in 0..overshoot {
            let _ = cached_regex(&format!("cliff-over-{i}-[a-z]+"), true).expect("compiles");
        }
        // Count how many of the ORIGINAL fill patterns are still cached. A clear() would leave 0.
        let survivors = (0..fill)
            .filter(|i| is_cached(&format!("cliff-fill-{i}-[a-z]+"), true))
            .count();
        assert!(
            survivors > fill / 2,
            "overflow must evict a batch, not flush the cache: only {survivors}/{fill} of the \
             pre-overflow entries survived (clear-on-overflow would leave 0)"
        );
    }

    // The lock-free store must be safe under concurrent hits and misses from many threads (papaya's
    // design center). This is a smoke test that the eviction/insert logic doesn't corrupt or panic
    // under contention; papaya provides the lock-freedom.
    #[test]
    fn concurrent_hits_and_misses_are_safe() {
        use std::thread;
        let threads: Vec<_> = (0..8)
            .map(|t| {
                thread::spawn(move || {
                    for i in 0..500 {
                        // A shared hot set (hits) plus per-thread distinct patterns (misses).
                        let _ = cached_regex("concurrent-hot-[0-9]+", false);
                        let _ = cached_regex(&format!("concurrent-{t}-{i}-[a-z]+"), false);
                    }
                })
            })
            .collect();
        for h in threads {
            h.join().expect("no panic under concurrent access");
        }
        // The shared hot pattern must still resolve and match.
        let re = cached_regex("concurrent-hot-[0-9]+", false).expect("compiles");
        assert!(re.is_match("concurrent-hot-42"));
    }
}
