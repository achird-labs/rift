//! An imposter's random stream (issue #1755): one SplitMix64, seeded with `_rift.scriptEngine.seed`
//! when set and from OS entropy otherwise, so behaviour and cost are the same either way.
//!
//! What draws from it: `_rift.fault` rolls, the template `randomInt`, and a wasm component's
//! `wasi:random` (`random`, `insecure`, `insecure-seed`). With a seed and a frozen clock (#1754), a
//! serialized request sequence replays byte for byte; concurrent requests interleave their draws
//! nondeterministically, as they would against any single stream. What does not: the `wait`
//! behaviour's numeric random range, which only changes timing. JavaScript's `Math.random()` draws
//! from it too (#1756, through `scripting::env`), a JS `wait` function's included; Rhai has no
//! random function.
//! The template `uuid` draws from it too.
//!
//! Readers draw through a request's [`RequestRng`], never the imposter's stream directly, which is
//! where request-keyed streams go (#1795). A fresh stream starts when the imposter is created,
//! recreated, or restarted (`restart_runtime_state`), so request *n* after any of them sees the
//! same draws; a request keeps the stream it started on.

use parking_lot::Mutex;
use std::convert::Infallible;
use std::sync::Arc;

/// SplitMix64 (Steele, Lea and Flood, 2014): a 64-bit state stepped by a fixed odd constant and
/// mixed. Fast, and every seed gives a full-period stream.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// `_rift.scriptEngine.seed`. Decoded tolerantly: engines before #1755 admitted any value under this
/// key and ignored it, so a stored config carrying one must still replay (rift-cluster's log,
/// `--datadir`). A value that is not an unsigned 64-bit integer is kept as a JSON value (an integer
/// past `u64` re-encodes as a float), runs unseeded, and is logged when the imposter is built.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum Seed {
    /// The seed.
    Value(u64),
    /// Anything else: runs unseeded.
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized(serde_json::Value),
}

impl Seed {
    /// The seed, or `None` for a value that is not one.
    #[must_use]
    pub fn value(&self) -> Option<u64> {
        match self {
            Self::Value(seed) => Some(*seed),
            Self::Unrecognized(_) => None,
        }
    }
}

/// `_rift.scriptEngine.rngKey` (issue #1795): which parts of a request name its own random
/// stream, so concurrent requests draw the same values whatever order they arrive in. Decoded
/// tolerantly, like [`Seed`]: engines before #1795 admitted any value here and ignored it. A value
/// that is not a list of strings, or names a part Rift cannot read, is refused at admission
/// ([`check_rng_key`]); replayed from storage, it is logged and the imposter's stream is used.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum RngKey {
    /// The parts: `header:<name>`, `query:<name>`, `method`, `path`, `body:<RFC 6901 pointer>`.
    Parts(Vec<String>),
    /// Anything else: refused at admission, ignored on replay.
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unrecognized(serde_json::Value),
}

/// One part of an `rngKey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPart {
    /// A request header, by name (case-insensitive); its first value.
    Header(String),
    /// A query parameter, by name; its first value.
    Query(String),
    Method,
    Path,
    /// A value in the JSON request body, by RFC 6901 pointer: a string as itself, anything else as
    /// JSON text.
    Body(String),
}

fn valid_pointer(pointer: &str) -> bool {
    if pointer.is_empty() {
        return true;
    }
    if !pointer.starts_with('/') {
        return false;
    }
    // `~` only as `~0` or `~1`.
    let bytes = pointer.as_bytes();
    bytes
        .iter()
        .enumerate()
        .all(|(i, b)| *b != b'~' || matches!(bytes.get(i + 1), Some(b'0' | b'1')))
}

impl RngKey {
    /// The parts, or why the value cannot be one.
    pub fn parts(&self) -> Result<Vec<KeyPart>, String> {
        let Self::Parts(parts) = self else {
            return Err("_rift.scriptEngine.rngKey must be a list of strings".into());
        };
        if parts.is_empty() {
            return Err("_rift.scriptEngine.rngKey must name at least one part".into());
        }
        parts
            .iter()
            .map(|part| {
                let parsed = if part == "method" {
                    Some(KeyPart::Method)
                } else if part == "path" {
                    Some(KeyPart::Path)
                } else if let Some(name) = part.strip_prefix("header:").filter(|n| !n.is_empty()) {
                    Some(KeyPart::Header(name.to_ascii_lowercase()))
                } else if let Some(name) = part.strip_prefix("query:").filter(|n| !n.is_empty()) {
                    Some(KeyPart::Query(name.to_owned()))
                } else if let Some(pointer) = part.strip_prefix("body:") {
                    if !valid_pointer(pointer) {
                        return Err(format!(
                            "_rift.scriptEngine.rngKey part {part:?}: not an RFC 6901 pointer"
                        ));
                    }
                    Some(KeyPart::Body(pointer.to_owned()))
                } else {
                    None
                };
                parsed.ok_or_else(|| {
                    format!(
                        "_rift.scriptEngine.rngKey part {part:?} is not one of header:<name>, \
                         query:<name>, method, path, body:<pointer>"
                    )
                })
            })
            .collect()
    }
}

/// Admission (issue #1795): an `rngKey` must parse and needs a `seed`, without which it would
/// change nothing. Never run on decode: a stored config replays whatever it holds.
pub fn check_rng_key(seed: Option<&Seed>, rng_key: Option<&RngKey>) -> Result<(), String> {
    let Some(key) = rng_key else {
        return Ok(());
    };
    key.parts()?;
    if seed.and_then(Seed::value).is_none() {
        return Err(
            "_rift.scriptEngine.rngKey needs _rift.scriptEngine.seed: without a seed it changes nothing"
                .into(),
        );
    }
    Ok(())
}

/// What a request offers an `rngKey`.
pub struct KeySource<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// A header's first value, by lowercase name.
    pub header: &'a dyn Fn(&str) -> Option<String>,
    /// A query parameter's first value, by name.
    pub query: &'a dyn Fn(&str) -> Option<String>,
    /// The body text.
    pub body: Option<&'a str>,
}

/// The digest of a request's key: each part's value, length-prefixed so `["a","bc"]` and
/// `["ab","c"]` differ, hashed with FNV-1a. `None` when every part is empty (a missing header, a
/// pointer that finds nothing): the request then uses the imposter's stream. A partly missing key
/// renders the missing part as empty.
#[must_use]
pub fn key_digest(parts: &[KeyPart], request: &KeySource<'_>) -> Option<u64> {
    let body_json: Option<serde_json::Value> = request
        .body
        // Domain-optional parse: a body that is not JSON has no pointer values.
        .and_then(|b| serde_json::from_str(b).ok());
    let values: Vec<String> = parts
        .iter()
        .map(|part| match part {
            KeyPart::Method => request.method.to_owned(),
            KeyPart::Path => request.path.to_owned(),
            KeyPart::Header(name) => (request.header)(name).unwrap_or_default(),
            KeyPart::Query(name) => (request.query)(name).unwrap_or_default(),
            KeyPart::Body(pointer) => body_json
                .as_ref()
                .and_then(|json| json.pointer(pointer))
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default(),
        })
        .collect();
    if values.iter().all(String::is_empty) {
        return None;
    }
    let mut hasher = crate::imposter::reconcile::Fnv1a::default();
    for value in &values {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    Some(hasher.finish())
}

/// SplitMix64's finalizer: spreads a value's bits, so short keys and small counters do not start
/// neighbouring streams.
const fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The starting state of a keyed stream: the seed, the key's digest and its occurrence, mixed.
#[must_use]
pub const fn keyed_state(seed: u64, digest: u64, occurrence: u32) -> u64 {
    mix(seed ^ mix(digest) ^ mix(occurrence as u64 + 1))
}

/// One imposter's stream.
#[derive(Debug)]
pub struct ImposterRng {
    seed: Option<u64>,
    stream: Mutex<SplitMix64>,
    /// How many requests with each `rngKey` digest have drawn a keyed stream since this stream
    /// began (issue #1795): a fresh imposter stream (create, recreate, restart) starts at zero.
    /// Grows by about 12 bytes per distinct key, with no cap.
    occurrences: Mutex<std::collections::HashMap<u64, u32>>,
}

fn entropy() -> u64 {
    rand::random()
}

impl ImposterRng {
    /// A stream from `seed`, or from OS entropy when `None`.
    #[must_use]
    pub fn new(seed: Option<u64>) -> Self {
        Self {
            seed,
            stream: Mutex::new(SplitMix64::new(seed.unwrap_or_else(entropy))),
            occurrences: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The 0-based count of earlier requests with `digest` since this stream began, counting this
    /// one (issue #1795).
    pub fn next_occurrence(&self, digest: u64) -> u32 {
        let mut occurrences = self.occurrences.lock();
        let slot = occurrences.entry(digest).or_insert(0);
        let occurrence = *slot;
        *slot = slot.saturating_add(1);
        occurrence
    }

    /// The configured seed.
    #[must_use]
    pub fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// Back to the start: the seed's first draw, or a fresh entropy seed, with no `rngKey`
    /// occurrences counted.
    pub fn reseed(&self) {
        *self.stream.lock() = SplitMix64::new(self.seed.unwrap_or_else(entropy));
        self.occurrences.lock().clear();
    }

    fn next_u64(&self) -> u64 {
        self.stream.lock().next_u64()
    }
}

/// A request's handle on its imposter's stream: what every reader draws through. A `rand` RNG
/// (`rand::Rng` via `TryRng<Error = Infallible>`), cheap to clone; clones share the stream, and
/// [`Self::rekey`] moves every clone at once onto a stream of the request's own (issue #1795).
#[derive(Debug, Clone)]
pub struct RequestRng(Arc<arc_swap::ArcSwap<ImposterRng>>);

impl RequestRng {
    #[must_use]
    pub fn new(imposter: Arc<ImposterRng>) -> Self {
        Self(Arc::new(arc_swap::ArcSwap::new(imposter)))
    }

    /// A handle on a stream of its own, seeded from OS entropy: for a caller with no imposter.
    #[must_use]
    pub fn unseeded() -> Self {
        Self::new(Arc::new(ImposterRng::new(None)))
    }

    /// Draw from `stream` from now on, this handle and every clone of it.
    pub fn rekey(&self, stream: ImposterRng) {
        self.0.store(Arc::new(stream));
    }

    /// Draw from the stream `other` draws from now, from now on: how a wasm store kept across
    /// requests (issue #1793) reads each request's stream through one handle.
    pub fn follow(&self, other: &Self) {
        self.0.store(other.0.load_full());
    }

    /// A `wasi:random/insecure-seed` value: two draws.
    pub fn seed_u128(&mut self) -> u128 {
        (u128::from(self.next()) << 64) | u128::from(self.next())
    }

    fn next(&self) -> u64 {
        self.0.load().next_u64()
    }
}

impl rand::TryRng for RequestRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        // The high half: SplitMix64's best-mixed bits.
        Ok((self.next() >> 32) as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(self.next())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        for chunk in dst.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Ok(())
    }
}

tokio::task_local! {
    /// The handle of the request this task serves.
    static REQUEST_RNG: RequestRng;
}

std::thread_local! {
    /// The same, for a blocking-pool thread running a request's work.
    static THREAD_RNG: std::cell::RefCell<Option<RequestRng>> = const { std::cell::RefCell::new(None) };
}

/// The handle of the request being served here, on its task or on a blocking thread running its
/// work; `None` outside a request.
#[must_use]
pub fn current() -> Option<RequestRng> {
    REQUEST_RNG
        .try_with(Clone::clone)
        .ok()
        .or_else(|| THREAD_RNG.with(|slot| slot.borrow().clone()))
}

/// Run `fut` with `rng` as its request's handle.
pub async fn with_request_rng<F: std::future::Future>(rng: RequestRng, fut: F) -> F::Output {
    REQUEST_RNG.scope(rng, fut).await
}

/// Run `f` on this thread with `rng` as [`current`], for a closure handed to the blocking pool.
/// The thread's previous handle comes back after, a panicking `f` included.
pub fn carry_request_rng<T>(rng: Option<RequestRng>, f: impl FnOnce() -> T) -> T {
    struct Restore(Option<RequestRng>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            THREAD_RNG.with(|slot| *slot.borrow_mut() = previous);
        }
    }
    let _restore = Restore(THREAD_RNG.with(|slot| slot.replace(rng)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{RngExt, TryRng};

    // Published SplitMix64 outputs for seed 0 (e.g. the reference C, `next()` from x = 0).
    #[test]
    fn splitmix64_matches_the_reference_vectors() {
        let mut s = SplitMix64::new(0);
        assert_eq!(s.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(s.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(s.next_u64(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn a_seed_repeats_after_reseed_and_entropy_does_not() {
        let seeded = Arc::new(ImposterRng::new(Some(42)));
        let mut rng = RequestRng::new(Arc::clone(&seeded));
        let first: Vec<u64> = (0..4).map(|_| rng.try_next_u64().unwrap_or(0)).collect();
        seeded.reseed();
        let again: Vec<u64> = (0..4).map(|_| rng.try_next_u64().unwrap_or(0)).collect();
        assert_eq!(first, again);
        let mut other = RequestRng::new(Arc::new(ImposterRng::new(Some(43))));
        assert_ne!(first[0], other.try_next_u64().unwrap_or(0));
        // Unseeded: two fresh streams start apart (a 1-in-2^64 chance otherwise).
        let mut a = RequestRng::unseeded();
        let mut b = RequestRng::unseeded();
        assert_ne!(a.try_next_u64().ok(), b.try_next_u64().ok());
    }

    #[test]
    fn bytes_and_ranges_draw_from_the_stream() {
        let mut a = RequestRng::new(Arc::new(ImposterRng::new(Some(7))));
        let mut b = RequestRng::new(Arc::new(ImposterRng::new(Some(7))));
        let (mut x, mut y) = ([0u8; 13], [0u8; 13]);
        a.try_fill_bytes(&mut x).ok();
        b.try_fill_bytes(&mut y).ok();
        assert_eq!(x, y);
        assert_eq!(a.random_range(0..=1000), b.random_range(0..=1000));
        assert_eq!(a.seed_u128(), b.seed_u128());
    }

    #[test]
    fn rng_keys_parse_and_refuse_what_they_cannot_read() {
        let key = |v: serde_json::Value| -> RngKey { serde_json::from_value(v).expect("decodes") };
        assert_eq!(
            key(serde_json::json!([
                "header:X-Rift-Rng-Key",
                "query:id",
                "method",
                "path",
                "body:/a~1b/0"
            ]))
            .parts()
            .expect("parses"),
            vec![
                KeyPart::Header("x-rift-rng-key".into()),
                KeyPart::Query("id".into()),
                KeyPart::Method,
                KeyPart::Path,
                KeyPart::Body("/a~1b/0".into())
            ]
        );
        for (bad, why) in [
            (serde_json::json!([]), "at least one"),
            (serde_json::json!(["cookie:x"]), "is not one of"),
            (serde_json::json!(["header:"]), "is not one of"),
            (serde_json::json!(["body:a/b"]), "RFC 6901"),
            (serde_json::json!(["body:/a~2"]), "RFC 6901"),
            (serde_json::json!("header:x"), "list of strings"),
        ] {
            let err = key(bad.clone()).parts().expect_err("refused");
            assert!(err.contains(why), "{bad}: {err}");
        }
        let seed = Seed::Value(1);
        let ok = key(serde_json::json!(["path"]));
        assert!(check_rng_key(Some(&seed), Some(&ok)).is_ok());
        assert!(
            check_rng_key(None, Some(&ok))
                .expect_err("no seed")
                .contains("needs")
        );
        assert!(check_rng_key(None, None).is_ok());
    }

    #[test]
    fn digests_separate_parts_and_skip_an_empty_key() {
        let none = |_: &str| None;
        let source = |method: &'static str, path: &'static str| KeySource {
            method,
            path,
            header: &none,
            query: &none,
            body: None,
        };
        let two = [KeyPart::Method, KeyPart::Path];
        // ["a","bc"] and ["ab","c"] do not collide.
        assert_ne!(
            key_digest(&two, &source("a", "bc")),
            key_digest(&two, &source("ab", "c"))
        );
        assert_eq!(
            key_digest(&two, &source("a", "bc")),
            key_digest(&two, &source("a", "bc"))
        );
        // Every part empty: no key.
        assert_eq!(
            key_digest(&[KeyPart::Header("x".into())], &source("", "")),
            None
        );
        // A body pointer reads a string as itself.
        let body = KeySource {
            body: Some(r#"{"id":"abc","n":7}"#),
            ..source("", "")
        };
        assert!(key_digest(&[KeyPart::Body("/id".into())], &body).is_some());
        assert_ne!(
            key_digest(&[KeyPart::Body("/id".into())], &body),
            key_digest(&[KeyPart::Body("/n".into())], &body)
        );
        // Occurrences give different streams of one key.
        assert_ne!(keyed_state(42, 7, 0), keyed_state(42, 7, 1));
        assert_ne!(keyed_state(42, 7, 0), keyed_state(43, 7, 0));
    }

    #[test]
    fn a_rekeyed_handle_moves_every_clone() {
        let mut a = RequestRng::new(Arc::new(ImposterRng::new(Some(1))));
        let mut b = a.clone();
        a.rekey(ImposterRng::new(Some(99)));
        let mut fresh = RequestRng::new(Arc::new(ImposterRng::new(Some(99))));
        assert_eq!(b.try_next_u64().ok(), fresh.try_next_u64().ok());
        assert_eq!(a.try_next_u64().ok(), fresh.try_next_u64().ok());
        let imposter = ImposterRng::new(Some(1));
        assert_eq!(
            (
                imposter.next_occurrence(5),
                imposter.next_occurrence(5),
                imposter.next_occurrence(6)
            ),
            (0, 1, 0)
        );
    }

    #[test]
    fn a_seed_decodes_tolerantly() {
        let seed: Seed = serde_json::from_value(serde_json::json!(42)).expect("decodes");
        assert_eq!(seed.value(), Some(42));
        for odd in [
            serde_json::json!(-1),
            serde_json::json!("42"),
            serde_json::json!(1.5),
        ] {
            let seed: Seed = serde_json::from_value(odd.clone()).expect("decodes");
            assert_eq!(seed.value(), None, "{odd}");
            assert_eq!(
                serde_json::to_value(&seed).expect("encodes"),
                odd,
                "kept as written"
            );
        }
    }

    #[tokio::test]
    async fn the_handle_reaches_its_task_and_a_carried_thread_only() {
        assert!(current().is_none());
        let rng = RequestRng::new(Arc::new(ImposterRng::new(Some(1))));
        let seen = with_request_rng(rng, async {
            let carried = current();
            let on_thread = tokio::task::spawn_blocking(move || {
                carry_request_rng(carried, || current().is_some())
            })
            .await
            .expect("join");
            let uncarried = tokio::task::spawn_blocking(|| current().is_some())
                .await
                .expect("join");
            (current().is_some(), on_thread, uncarried)
        })
        .await;
        assert_eq!(seen, (true, true, false));
        assert!(current().is_none());
    }
}
