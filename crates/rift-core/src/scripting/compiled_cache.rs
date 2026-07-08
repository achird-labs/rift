//! Content-addressed compiled-script cache for the imposter `_rift.script` path (issue #356).
//!
//! `file:`/`ref:` script resolution (`imposter::script_resolve`) funnels every script down to a
//! plain `code` string before it reaches execution, so two responses that reference the same
//! file — directly, or both via `ref:` — end up with byte-identical `code`. Caching the compiled
//! Rhai AST by that content (rather than by path or rule id) lets them share one compiled entry,
//! and means editing the file changes the content and therefore the cache key, busting the entry
//! automatically — no explicit invalidation needed.
//!
//! The cache is **bounded** (issue #356 B3): without a cap, every distinct script content — every
//! hot-reload edit included — would leak one `Arc<AST>` for the process lifetime. On reaching
//! capacity the cache is cleared wholesale before the next insert (a simple, adequate policy; a
//! full LRU is overkill for a config-scale working set). Each entry also stores its source string
//! and verifies it on a hit (issue #356 B4): a `u64` hash collision would otherwise silently
//! return the WRONG compiled AST, so a mismatch recompiles instead.

use anyhow::{Result, anyhow};
use rhai::AST;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock, RwLock};

/// Maximum number of distinct compiled scripts retained. Comfortably above any realistic
/// imposter/config working set, while bounding memory against an adversarial or churny workload
/// (e.g. repeated hot-reload edits).
const DEFAULT_CAPACITY: usize = 512;

fn content_key(code: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    code.hash(&mut hasher);
    hasher.finish()
}

/// A bounded, content-addressed cache of compiled Rhai ASTs. Each entry stores the source string
/// alongside the compiled `Arc<AST>` so a hit can verify the source matches (collision guard).
struct CompiledCache {
    entries: HashMap<u64, (String, Arc<AST>)>,
    capacity: usize,
}

impl CompiledCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity,
        }
    }

    /// Return the cached AST for `code` only when an entry under its key exists AND its stored
    /// source matches `code` — so a hash collision (same key, different source) is a miss, never a
    /// wrong-AST hit (B4).
    fn get_verified(&self, key: u64, code: &str) -> Option<Arc<AST>> {
        self.entries
            .get(&key)
            .and_then(|(source, ast)| (source == code).then(|| Arc::clone(ast)))
    }

    /// Return the cached AST for `code`, compiling and inserting it on a (verified) miss. Enforces
    /// the capacity bound: when inserting a NEW key would exceed `capacity`, the cache is cleared
    /// first (B3).
    fn get_or_compile(&mut self, key: u64, code: &str) -> Result<Arc<AST>> {
        if let Some(ast) = self.get_verified(key, code) {
            return Ok(ast);
        }
        let engine = rhai::Engine::new();
        let ast = Arc::new(
            engine
                .compile(code)
                .map_err(|e| anyhow!("Failed to compile script: {e}"))?,
        );
        // Only a brand-new key grows the map; replacing a same-key entry (a collision, or an
        // edited script re-hashed to the same slot) does not, so no clear is needed there.
        if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
            self.entries.clear();
        }
        self.entries
            .insert(key, (code.to_string(), Arc::clone(&ast)));
        Ok(ast)
    }
}

fn rhai_cache() -> &'static RwLock<CompiledCache> {
    static CACHE: OnceLock<RwLock<CompiledCache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(CompiledCache::new(DEFAULT_CAPACITY)))
}

/// Get a compiled Rhai AST for `code`, compiling and caching it on first use. A later call with
/// byte-identical content returns the same cached `Arc` without recompiling; different content
/// (e.g. an edited file) hashes to a different key and compiles fresh. Bounded and
/// collision-guarded — see the module docs.
pub(crate) fn cached_rhai_ast(code: &str) -> Result<Arc<AST>> {
    let key = content_key(code);
    // Fast path: a shared read lock is enough for a verified hit.
    {
        let cache = rhai_cache()
            .read()
            .map_err(|_| anyhow!("rhai script cache lock poisoned"))?;
        if let Some(ast) = cache.get_verified(key, code) {
            return Ok(ast);
        }
    }
    // Slow path: compile + insert under a write lock (get_or_compile re-checks for a hit first, so
    // a concurrent insert between the two locks does not double-compile a stored entry).
    let mut cache = rhai_cache()
        .write()
        .map_err(|_| anyhow!("rhai script cache lock poisoned"))?;
    cache.get_or_compile(key, code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_shares_one_compiled_entry() {
        let code = "fn should_inject(request, flow_store) { #{ inject: false } }";
        let a = cached_rhai_ast(code).unwrap();
        let b = cached_rhai_ast(code).unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "byte-identical content must return the same cached AST"
        );
    }

    #[test]
    fn different_content_compiles_a_distinct_entry() {
        let a = cached_rhai_ast("fn should_inject(request, flow_store) { #{ inject: false } }")
            .unwrap();
        let b = cached_rhai_ast("fn should_inject(request, flow_store) { #{ inject: true, fault: `latency`, duration_ms: 1 } }")
            .unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different content must not share a cache entry"
        );
    }

    #[test]
    fn editing_content_busts_the_cache_key() {
        // Simulates "edit the file, reload": the resolved `code` string changes, so the second
        // call recompiles instead of reusing the first (stale) AST.
        let original =
            cached_rhai_ast("fn should_inject(request, flow_store) { #{ inject: false } }")
                .unwrap();
        let edited = cached_rhai_ast("fn should_inject(request, flow_store) { #{ inject: true, fault: `latency`, duration_ms: 2 } }").unwrap();
        assert!(!Arc::ptr_eq(&original, &edited));
    }

    // B3: the cache never grows past its capacity — inserting more distinct scripts than the cap
    // triggers a wholesale clear rather than unbounded growth. Uses an isolated cache instance so
    // it doesn't perturb (or get perturbed by) the process-global cache other tests share.
    #[test]
    fn cache_does_not_grow_past_capacity() {
        let cap = 8;
        let mut cache = CompiledCache::new(cap);
        for i in 0..(cap * 4) {
            let code =
                format!("fn should_inject(request, flow_store) {{ #{{ inject: false, n: {i} }} }}");
            let key = content_key(&code);
            cache.get_or_compile(key, &code).unwrap();
            assert!(
                cache.entries.len() <= cap,
                "cache size {} exceeded cap {cap} after {} inserts",
                cache.entries.len(),
                i + 1
            );
        }
    }

    // B4: a hit is only returned when the STORED SOURCE matches — a same-key/different-source
    // collision compiles fresh and replaces, never returns the wrong AST. Drives the key
    // explicitly (a real 64-bit collision is infeasible to construct) to exercise the guard.
    #[test]
    fn collision_guard_never_returns_wrong_ast() {
        let mut cache = CompiledCache::new(16);
        let src_a = "fn should_inject(request, flow_store) { #{ inject: false, tag: 1 } }";
        let src_b = "fn should_inject(request, flow_store) { #{ inject: false, tag: 2 } }";
        let forced_key = 0xC0FFEE_u64;

        let a = cache.get_or_compile(forced_key, src_a).unwrap();
        // Same key, different source: must recompile B, not return A.
        let b = cache.get_or_compile(forced_key, src_b).unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "collision must not return A's AST for B"
        );

        // The verified lookup only matches the currently-stored source (B), not the evicted A.
        assert!(cache.get_verified(forced_key, src_b).is_some());
        assert!(
            cache.get_verified(forced_key, src_a).is_none(),
            "a source that doesn't match the stored entry is a miss, not a wrong-AST hit"
        );
    }
}
