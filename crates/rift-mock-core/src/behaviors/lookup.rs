//! Lookup behavior - query external data sources.

use super::copy::CopySource;
use super::extraction::{ExtractionMethod, XPathSelectorError};
use super::request::RequestContext;
use super::spliced::{
    Spliced, SplicedHeaders, authored_headers, plain_headers, replace_response_token,
};
use crate::imposter::body_limits::{BodyGuard, BodyReadError, BudgetShare, CollectedBody};
use crate::imposter::{LookupFileError, ScriptFileAccess, open_lookup_file, resolve_lookup_path};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Lookup behavior - query external data source
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LookupBehavior {
    /// Key extraction from request
    pub key: LookupKey,
    /// Data source configuration
    #[serde(rename = "fromDataSource")]
    pub from_data_source: DataSource,
    /// Token to replace in response (e.g., "${RESULT}")
    pub into: String,
}

/// Key extraction configuration for lookup
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LookupKey {
    /// Request field to extract key from
    pub from: CopySource,
    /// Extraction method
    #[serde(rename = "using")]
    pub extraction: ExtractionMethod,
    /// Which of the selector's matches keys the row, as in Mountebank (issue #1240): for a regex,
    /// `0` is the whole match and `n` the `n`th capture group; for JSONPath/XPath, the `n`th
    /// selected value. Absent (or `null`) keeps Rift's default, the first capture group (or the
    /// whole match when the pattern has none) and the first selected value. Any JSON value is
    /// admitted and read as Mountebank reads it (issue #1582); see [`LookupIndex`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<LookupIndex>,
}

/// A lookup `key.index`, any JSON value, kept as written (issue #1582). Mountebank has no rule for
/// it and reads `keyValues[key.index || 0]`, so `0`, `""` and `false` are the first match, a
/// non-negative integer or a numeric string such as `"1"` picks that match, and anything else
/// finds no row.
//
// Kept as JSON so it prints back unchanged and every stored stub keeps its bytes and `stub_key`;
// resolved by `position` when the key is read.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(transparent)]
pub struct LookupIndex(pub serde_json::Value);

impl LookupIndex {
    /// The match position this index names, or `None` when it names none, so no row is found and
    /// the token is left, as when the position is past the matches:
    /// - a falsy value (`0`, `""`, `false`) is `|| 0`, the first position;
    /// - a non-negative integral number is that position (`1.0` is the property `"1"`);
    /// - a string that is a canonical array index (`"1"`, not `"01"` or `" 1"`) is that position;
    /// - anything else (`-1`, `1.5`, `"x"`, `true`, an array or object) is no property of the
    ///   match array.
    #[must_use]
    pub fn position(&self) -> Option<usize> {
        use serde_json::Value;
        match &self.0 {
            Value::Null | Value::Bool(false) => Some(0),
            Value::Number(n) => match n.as_u64() {
                Some(n) => usize::try_from(n).ok(),
                None => n
                    .as_f64()
                    .filter(|f| *f >= 0.0 && f.fract() == 0.0 && *f < 2f64.powi(53))
                    // A whole, non-negative f64 below 2^53 is exact, so the cast loses nothing.
                    .and_then(|f| usize::try_from(f as u64).ok()),
            },
            Value::String(s) if s.is_empty() => Some(0),
            Value::String(s) => {
                let canonical =
                    s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
                // Domain-optional: a string that is not a canonical index in range names a property
                // the match array does not have, which is no row, not an error.
                if canonical {
                    s.parse::<usize>().ok()
                } else {
                    None
                }
            }
            Value::Bool(true) | Value::Array(_) | Value::Object(_) => None,
        }
    }
}

impl From<usize> for LookupIndex {
    fn from(index: usize) -> Self {
        Self(serde_json::Value::from(index))
    }
}

impl LookupKey {
    /// The key this lookup reads the row by, or `None` when the request yields none. An absent
    /// request field is `undefined`, which a regex reads as text (issue #1583).
    ///
    /// # Errors
    ///
    /// An XPath `using` that fails when it runs (issue #1585).
    pub fn extract(&self, request: &RequestContext) -> Result<Option<String>, XPathSelectorError> {
        let Some(source) =
            super::copy::source_or_undefined(&self.extraction, self.from.extract(request))
        else {
            return Ok(None);
        };
        match &self.index {
            None => self.extraction.extract(&source),
            Some(index) => {
                // Domain-optional: an index that is no array position finds no row (issue #1582).
                let Some(position) = index.position() else {
                    return Ok(None);
                };
                Ok(self
                    .extraction
                    .matches(&source)?
                    .into_iter()
                    .nth(position)
                    .flatten())
            }
        }
    }
}

/// External data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DataSource {
    /// CSV data source
    pub csv: CsvDataSource,
}

/// CSV data source configuration
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CsvDataSource {
    /// Path to CSV file
    pub path: String,
    /// Column to use as lookup key
    #[serde(rename = "keyColumn")]
    pub key_column: String,
    /// Field delimiter (default: `,`). It may be several characters, as Mountebank's csv-parse
    /// allows, and an empty string means `,` (issue #1566).
    #[serde(default = "default_delimiter")]
    pub delimiter: String,
}

fn default_delimiter() -> String {
    ",".to_string()
}

/// A `_rift.dataset` block: a [`LookupBehavior`] named by *dataset* instead of by file path.
///
/// **Carried, never executed.** Nothing in this crate reads this type beyond serializing it back
/// out, and a standalone Rift serves a response bearing one exactly as if it were absent. It
/// exists because the path a `lookup` needs is node-local, so a clustered deployment cannot put
/// one in a config that replicates: `rift-cluster` resolves the name to a content-addressed file
/// on each node and rewrites this block into a real [`LookupBehavior`] at apply time. The engine
/// then runs the same unmodified lookup code it always has.
///
/// Keeping the declarative form in the config — rather than only the compiled result — is what
/// lets the cluster hand back the binding the operator actually wrote, and is why this type is
/// part of the config schema rather than an internal detail of the cluster.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DatasetBinding {
    /// The dataset's name, unique per tenant.
    pub name: String,
    /// Which version to bind. Absent means "the latest at bind time"; the binder is expected to
    /// resolve it to a concrete version and record that here, so a later upload cannot silently
    /// change what a serving stub returns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// Where the lookup key comes from — the same extraction a [`LookupBehavior`] uses, so the
    /// compiled form carries this across unchanged.
    pub key: LookupKey,
    /// The dataset column matched against the key. Distinct from a CSV path's `keyColumn` only in
    /// that a dataset *declares* its key columns, so a binder can reject an unknown one up front.
    #[serde(rename = "keyColumn")]
    pub key_column: String,
    /// The token replaced in the response, e.g. `${row}`.
    pub into: String,
    /// The digest of the exact bytes this binding resolved to.
    ///
    /// Written by the binder when it pins `version`, not by the author of the config. It is the
    /// pin: two nodes compiling the same block reach the same file because they agree on this,
    /// not because they independently re-resolve a name whose latest version may have moved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// Parsed CSV files, keyed by the canonical path read, the delimiter it was parsed with (an empty
/// one is `,`) and whether the read was confined: two lookups that read one file with different
/// delimiters each get their own parse, as each gets its own in Mountebank, which re-reads the file
/// per request (issues #1566, #1617). A parse read under a trusted access is never served to a
/// confined one.
///
/// A file's path is resolved under the imposter's [`ScriptFileAccess`] on every use, before the
/// cache is consulted, so a path or link that has moved out of `--scripts-dir` since is refused
/// even when its earlier content is cached. A cached file whose identity, length or modification
/// time has changed since it was read is read again, so an edited file is seen on the next request,
/// as in Mountebank (issue #1654).
///
/// The cache holds at most the body ceiling (`--max-body-size`) of entries, each charged its
/// parse ([`CsvData::heap_bytes`]), its key and a fixed per-entry overhead, so even an empty parse
/// counts. Past the bound the least recently used entries are evicted (issue #1654). Each call
/// names the ceiling of its own imposter's [`BodyGuard`], so with several managers sharing the
/// cache the smallest ceiling in use wins. An entry larger than the bound by itself is served and
/// not cached, its parse held against the body memory budget until its row is served. Mountebank
/// has no cache at all, so an evicted file costs only the re-read Mountebank does on every request.
///
/// Concurrent misses on one file share one read and parse (issue #1715): the first starts the load
/// and the rest wait for it and take its outcome, a failure included, instead of each reading the
/// file under its own share of the budget.
pub struct CsvCache {
    entries: Mutex<CacheEntries>,
    /// The loads in progress, one per file version and budget. An entry lives while a request
    /// waits on it and is removed by the last one, so an outcome, a failure above all, is never
    /// remembered past its flight.
    flights: Mutex<HashMap<FlightKey, Flight>>,
    #[cfg(test)]
    loads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    gate: Mutex<Option<Arc<tests::Gate>>>,
}

/// A load in progress: the requests waiting on it share its one outcome.
type Flight = Arc<tokio::sync::OnceCell<LoadOutcome>>;

/// What a flight loads: one version of a file ([`CacheKey`], [`FileStamp`]) under one budget, named
/// by its [`BodyGuard`]'s address, so a flight never charges, or bounds by, another manager's.
type FlightKey = (CacheKey, FileStamp, usize);

/// What a `lookup` gets for its CSV file from [`CsvCache::fetch`].
#[derive(Debug, Clone)]
pub enum LoadOutcome {
    /// The parse, or `None` when the file may not or cannot be loaded (logged).
    Loaded(Option<CsvHandle>),
    /// The budget cannot hold the file now: a `503`.
    Exhausted(LookupLoadError),
    /// The load panicked, with the panic's description: a defect, not a data source fault.
    Panicked(String),
}

/// What a cache entry costs besides its parse and its key's heap bytes: the key, shared by the map
/// and the recency index through one `Arc`, the two `Arc` headers (key and parse), the entry, the
/// parse's struct, and the slots holding them.
const ENTRY_OVERHEAD: usize = std::mem::size_of::<CacheKey>()
    + 2 * std::mem::size_of::<Arc<CacheKey>>()
    + 4 * std::mem::size_of::<usize>()
    + std::mem::size_of::<CacheEntry>()
    + std::mem::size_of::<CsvData>()
    + 64;

/// How many keys are remembered as already warned about, or as refused for an unchanged file;
/// past it the record starts over, so the warnings and re-reads, not the memory, repeat.
const MAX_WARNED: usize = 1024;

/// Whether a parse was read under a trusted access or a confined one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ReadMode {
    Trusted,
    Confined,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    path: PathBuf,
    delimiter: String,
    mode: ReadMode,
}

impl CacheKey {
    fn new(path: PathBuf, delimiter: &str, access: &ScriptFileAccess) -> Self {
        // Mountebank passes `csvConfig.delimiter || ','`, so an empty delimiter is a comma, and
        // both name one parse.
        let delimiter = if delimiter.is_empty() { "," } else { delimiter };
        let mode = match access {
            ScriptFileAccess::Trusted { .. } => ReadMode::Trusted,
            ScriptFileAccess::Confined(_) | ScriptFileAccess::Refused => ReadMode::Confined,
        };
        Self {
            path,
            delimiter: delimiter.to_string(),
            mode,
        }
    }

    /// What an entry for this key holding a parse of `data_bytes` costs the cache.
    fn charge(&self, data_bytes: usize) -> usize {
        data_bytes + self.path.as_os_str().len() + self.delimiter.capacity() + ENTRY_OVERHEAD
    }
}

/// The cached files, the bytes they are charged, their recency order, and the oversized keys
/// already warned about.
#[derive(Default)]
struct CacheEntries {
    files: HashMap<Arc<CacheKey>, CacheEntry>,
    /// Last use → key, oldest first, so eviction is `O(log n)`. The key is the map's own `Arc`.
    recency: BTreeMap<u64, Arc<CacheKey>>,
    total_bytes: usize,
    clock: u64,
    /// Hashes, not keys: a key's delimiter may be long, and this set is not charged to the bound.
    warned: HashSet<u64>,
    /// Key hash → the file a parse too large for the whole budget was read from, so the same
    /// unchanged file is refused without being read again.
    refused: HashMap<u64, FileStamp>,
    hasher: std::hash::RandomState,
}

impl CacheEntries {
    fn remove(&mut self, key: &CacheKey) -> Option<CacheEntry> {
        let entry = self.files.remove(key)?;
        self.recency.remove(&entry.last_used);
        self.total_bytes -= entry.bytes;
        Some(entry)
    }
}

struct CacheEntry {
    data: Arc<CsvData>,
    stamp: FileStamp,
    bytes: usize,
    last_used: u64,
}

/// What a file looked like when it was read: a different file, length or modification time on a
/// later use means it has been replaced or edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    /// Device and inode, on Unix: a file swapped in with the same length and time is still a
    /// different file.
    identity: Option<(u64, u64)>,
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            Some((metadata.dev(), metadata.ino()))
        };
        #[cfg(not(unix))]
        let identity = None;
        Self {
            len: metadata.len(),
            // Domain-optional: a platform without modification times compares the rest alone.
            modified: metadata.modified().ok(),
            identity,
        }
    }
}

/// One lookup's parse of a CSV file. A parse too large for the cache also holds its share of the
/// body memory budget until this is dropped, once its row is served (issue #1654); the requests of
/// one flight share the parse and the share (issue #1715), released with the last of them.
#[derive(Clone)]
pub struct CsvHandle {
    data: Arc<CsvData>,
    _share: Option<Arc<BudgetShare>>,
}

impl fmt::Debug for CsvHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CsvHandle")
            .field("data_bytes", &self.data.heap_bytes())
            .field("holds_budget", &self._share.is_some())
            .finish()
    }
}

impl CsvHandle {
    /// The parse.
    #[must_use]
    pub fn data(&self) -> &Arc<CsvData> {
        &self.data
    }
}

impl std::ops::Deref for CsvHandle {
    type Target = CsvData;

    fn deref(&self) -> &CsvData {
        &self.data
    }
}

/// Why a lookup cannot load its CSV file right now (issue #1654). A file that cannot be loaded at
/// all is not an error here: it is logged and the lookup's tokens are left in place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupLoadError {
    /// The body memory budget cannot hold the file while it is read, or a parse too large to cache
    /// while it is served: transient server load, so the request is answered `503` with
    /// `Retry-After`.
    #[error(
        "not enough body memory is free to read lookup CSV data source '{path}'; retry shortly"
    )]
    BudgetExhausted { path: String },
}

impl Default for CsvCache {
    fn default() -> Self {
        Self::new()
    }
}

impl CsvCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(CacheEntries::default()),
            flights: Mutex::new(HashMap::new()),
            #[cfg(test)]
            loads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            gate: Mutex::new(None),
        }
    }

    /// The parsed CSV file `path` names under `access`, for the serve path: the cached parse while
    /// the file is unchanged; otherwise a [`CsvCache::load`] off the async worker, shared with every
    /// concurrent request that misses on the same version of the file under the same `guard`
    /// (issue #1715). The waiters do not hold a blocking thread. If the request running the load is
    /// cancelled, the next waiter runs it.
    pub async fn fetch(
        &'static self,
        path: &str,
        delimiter: &str,
        access: Arc<ScriptFileAccess>,
        guard: Arc<BodyGuard>,
    ) -> LoadOutcome {
        if let Some(handle) = self.get_fresh(path, delimiter, &access) {
            return LoadOutcome::Loaded(Some(handle));
        }
        let load = || self.load_off_worker(path, delimiter, &access, &guard);
        // A path that does not resolve or a file that cannot be stat'ed is not shared: `load`
        // fails fast and logs why, so there is nothing to save by waiting on another request.
        let Ok(resolved) = resolve_lookup_path(path, &access) else {
            return load().await;
        };
        let Ok(metadata) = std::fs::metadata(&resolved) else {
            return load().await;
        };
        let key = (
            CacheKey::new(resolved, delimiter, &access),
            FileStamp::of(&metadata),
            Arc::as_ptr(&guard) as usize,
        );
        FlightHolder::join(&self.flights, key).outcome(load).await
    }

    /// [`CsvCache::load`] on a blocking thread, as its outcome.
    async fn load_off_worker(
        &'static self,
        path: &str,
        delimiter: &str,
        access: &Arc<ScriptFileAccess>,
        guard: &Arc<BodyGuard>,
    ) -> LoadOutcome {
        let (path, delimiter) = (path.to_string(), delimiter.to_string());
        let (access, guard) = (Arc::clone(access), Arc::clone(guard));
        match tokio::task::spawn_blocking(move || self.load(&path, &delimiter, &access, &guard))
            .await
        {
            Ok(Ok(handle)) => LoadOutcome::Loaded(handle),
            Ok(Err(e)) => LoadOutcome::Exhausted(e),
            Err(e) => LoadOutcome::Panicked(e.to_string()),
        }
    }

    /// The parsed CSV file `path` names under `access`: the cached parse while the file is
    /// unchanged, otherwise read under `guard` with [`CsvCache::load`].
    ///
    /// `Ok(None)` when the path may not be read under `access` (issue #1617) or the file cannot be
    /// loaded; either is logged, so a lookup's tokens are never left in place silently.
    ///
    /// # Errors
    /// [`LookupLoadError::BudgetExhausted`] when `guard`'s budget cannot hold the file now.
    pub fn get_or_load(
        &self,
        path: &str,
        delimiter: &str,
        access: &ScriptFileAccess,
        guard: &BodyGuard,
    ) -> Result<Option<CsvHandle>, LookupLoadError> {
        if let Some(data) = self.get_fresh(path, delimiter, access) {
            return Ok(Some(data));
        }
        self.load(path, delimiter, access, guard)
    }

    /// The cached parse of `path` with `delimiter`, if there is one and the file has not changed
    /// since it was read. A changed file's parse is dropped. Does no more file I/O than resolving
    /// the path and one `stat`, so it runs on the caller's thread; a miss is loaded with
    /// [`CsvCache::load`].
    pub fn get_fresh(
        &self,
        path: &str,
        delimiter: &str,
        access: &ScriptFileAccess,
    ) -> Option<CsvHandle> {
        // Not an error here: a path that does not resolve or a file that cannot be stat'ed is a
        // miss, and `load` resolves it again and logs why it cannot be read.
        let resolved = resolve_lookup_path(path, access).ok()?;
        let stamp = FileStamp::of(&std::fs::metadata(&resolved).ok()?);
        let key = CacheKey::new(resolved, delimiter, access);
        let stale = {
            let mut entries = self.entries.lock();
            entries.clock += 1;
            let now = entries.clock;
            let fresh = entries.files.get_key_value(&key).map(|(shared, entry)| {
                (entry.stamp == stamp).then(|| (Arc::clone(shared), entry.last_used))
            })?;
            if let Some((shared, last)) = fresh {
                let entry = entries.files.get_mut(&key)?;
                entry.last_used = now;
                let data = Arc::clone(&entry.data);
                entries.recency.remove(&last);
                entries.recency.insert(now, shared);
                return Some(CsvHandle { data, _share: None });
            }
            entries.remove(&key)
        };
        // Dropped outside the lock: freeing a large parse is not done while others wait on it.
        drop(stale);
        None
    }

    /// Read and parse the CSV file `path` names under `access`, holding a share of `guard`'s body
    /// memory budget for the file's bytes while it is read and parsed (issue #1654), and cache the
    /// parse. A parse too large to cache instead holds its own share until the returned handle is
    /// dropped, so the budget bounds the uncached parses in flight; it is logged once per file.
    ///
    /// Blocking file I/O and a full parse, so the serve path runs it off the async worker.
    ///
    /// `Ok(None)` when the file may not or cannot be read, or is larger than `guard`'s ceiling
    /// (`--max-body-size`); each is logged, and the lookup's tokens are left in place.
    ///
    /// # Errors
    /// [`LookupLoadError::BudgetExhausted`] when `guard`'s budget cannot hold the file now.
    pub fn load(
        &self,
        path: &str,
        delimiter: &str,
        access: &ScriptFileAccess,
        guard: &BodyGuard,
    ) -> Result<Option<CsvHandle>, LookupLoadError> {
        #[cfg(test)]
        self.test_hooks();
        #[cfg(test)]
        assert!(!path.ends_with("panic.csv"), "test-induced load panic");
        let exhausted = || LookupLoadError::BudgetExhausted {
            path: path.to_string(),
        };
        // A failure here means a misconfigured data source (missing/unreadable/malformed CSV);
        // surface it instead of silently serving the response with the lookup tokens left
        // unreplaced.
        let unreadable = |source: std::io::Error| -> Result<Option<CsvHandle>, LookupLoadError> {
            let e = LookupFileError::Unreadable {
                path: path.to_string(),
                source,
            };
            tracing::warn!("lookup behavior: {e}");
            Ok(None)
        };
        let resolved = match resolve_lookup_path(path, access) {
            Ok(resolved) => resolved,
            Err(e) => {
                tracing::warn!("lookup behavior: {e}");
                return Ok(None);
            }
        };
        let mut file = match open_lookup_file(path, &resolved, access) {
            Ok(file) => file,
            Err(e) => {
                tracing::warn!("lookup behavior: {e}");
                return Ok(None);
            }
        };
        let stamp = match file.metadata() {
            Ok(metadata) => FileStamp::of(&metadata),
            Err(e) => return unreadable(e),
        };
        let key = CacheKey::new(resolved, delimiter, access);
        // Refused before and unchanged since: already logged, so not read or logged again.
        if self.refused_before(&key, stamp) {
            return Ok(None);
        }
        let bytes = match read_under(&mut file, stamp.len, guard) {
            Ok(bytes) => bytes,
            Err(BodyReadError::BudgetExhausted) => return Err(exhausted()),
            Err(BodyReadError::TooLarge { limit }) => {
                tracing::warn!(
                    "lookup behavior: CSV data source '{path}' ({} bytes) is larger than \
                     --max-body-size ({limit} bytes), so it is not read and the tokens are left in \
                     place",
                    stamp.len
                );
                return Ok(None);
            }
            Err(BodyReadError::Read(e)) => return unreadable(std::io::Error::other(e)),
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return unreadable(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            ));
        };
        let data = match CsvData::from_text(text, path, delimiter) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!("lookup behavior: failed to load CSV data source '{path}': {e}");
                return Ok(None);
            }
        };
        let data_bytes = data.heap_bytes();
        let charge = key.charge(data_bytes);
        let bound = guard.limits().max_body_bytes();
        if charge <= bound {
            // The file's bytes, and their share of the budget, are released once parsed.
            drop(bytes);
            let data = Arc::new(data);
            self.insert(key, stamp, Arc::clone(&data), charge, bound);
            return Ok(Some(CsvHandle { data, _share: None }));
        }
        // Too large to cache: the parse holds a share of its own while it is served, taken while
        // the file's share is still held.
        let share = match guard.try_reserve(data_bytes) {
            Ok(share) => share,
            Err(BodyReadError::BudgetExhausted) => return Err(exhausted()),
            Err(e) => {
                if self.first_warning(&key, Some(stamp)) {
                    tracing::warn!(
                        "lookup behavior: CSV data source '{path}' parses to about {data_bytes} \
                         bytes, more than --body-memory-budget can hold ({e}), so it is not served \
                         and the tokens are left in place until the file changes"
                    );
                }
                return Ok(None);
            }
        };
        drop(bytes);
        if self.first_warning(&key, None) {
            tracing::warn!(
                "lookup behavior: CSV data source '{path}' parses to about {data_bytes} bytes, more \
                 than the lookup cache holds (--max-body-size, {bound} bytes), so it is not cached \
                 and is read again on every use"
            );
        }
        Ok(Some(CsvHandle {
            data: Arc::new(data),
            _share: Some(Arc::new(share)),
        }))
    }

    /// Cache `data` under `key`, charged `bytes`, evicting the least recently used entries until
    /// the cache holds at most `bound` bytes. `bytes` is at most `bound`.
    fn insert(
        &self,
        key: CacheKey,
        stamp: FileStamp,
        data: Arc<CsvData>,
        bytes: usize,
        bound: usize,
    ) {
        let mut dropped = Vec::new();
        {
            let mut entries = self.entries.lock();
            if let Some(replaced) = entries.remove(&key) {
                dropped.push(replaced);
            }
            entries.clock += 1;
            let now = entries.clock;
            let key = Arc::new(key);
            entries.recency.insert(now, Arc::clone(&key));
            entries.files.insert(
                key,
                CacheEntry {
                    data,
                    stamp,
                    bytes,
                    last_used: now,
                },
            );
            entries.total_bytes += bytes;
            // Never the entry just inserted while others remain: it is the most recently used,
            // and alone it fits the bound.
            while entries.total_bytes > bound {
                let Some((_, oldest)) = entries.recency.pop_first() else {
                    break;
                };
                if let Some(evicted) = entries.files.remove(&oldest) {
                    entries.total_bytes -= evicted.bytes;
                    dropped.push(evicted);
                }
            }
        }
        // Dropped outside the lock: freeing evicted parses is not done while others wait on it.
        drop(dropped);
    }

    /// Whether this is the first warning for `key`, so it is logged once rather than on every
    /// read. With `refused`, the file is remembered as refused while it stays unchanged, and the
    /// warning is first again once it changes.
    fn first_warning(&self, key: &CacheKey, refused: Option<FileStamp>) -> bool {
        let mut entries = self.entries.lock();
        if entries.warned.len() >= MAX_WARNED {
            entries.warned.clear();
        }
        if entries.refused.len() >= MAX_WARNED {
            entries.refused.clear();
        }
        let hash = std::hash::BuildHasher::hash_one(&entries.hasher, key);
        match refused {
            // A changed file refused again is warned about again.
            Some(stamp) => entries.refused.insert(hash, stamp) != Some(stamp),
            None => entries.warned.insert(hash),
        }
    }

    /// Whether `key`'s file, as `stamp` describes it, was refused before ([`Self::first_warning`]).
    fn refused_before(&self, key: &CacheKey, stamp: FileStamp) -> bool {
        let entries = self.entries.lock();
        let hash = std::hash::BuildHasher::hash_one(&entries.hasher, key);
        entries.refused.get(&hash) == Some(&stamp)
    }

    /// The bytes the cached entries are charged.
    #[must_use]
    pub fn cached_bytes(&self) -> usize {
        self.entries.lock().total_bytes
    }

    /// How many parses are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().files.len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear cache
    pub fn clear(&self) {
        let dropped = {
            let mut entries = self.entries.lock();
            entries.recency.clear();
            entries.total_bytes = 0;
            std::mem::take(&mut entries.files)
        };
        drop(dropped);
    }
}

#[cfg(test)]
impl CsvCache {
    /// Count the load, then wait at the gate a test closed, if any.
    fn test_hooks(&self) {
        self.loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let gate = self.gate.lock().clone();
        if let Some(gate) = gate {
            gate.wait();
        }
    }

    /// Hold every load at its start until the returned gate is opened.
    fn close_gate(&self) -> Arc<tests::Gate> {
        let gate = Arc::new(tests::Gate::default());
        *self.gate.lock() = Some(Arc::clone(&gate));
        gate
    }

    fn load_count(&self) -> usize {
        self.loads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The requests waiting on a flight, across every flight.
    fn flight_holders(&self) -> usize {
        self.flights
            .lock()
            .values()
            .map(|cell| Arc::strong_count(cell) - 1)
            .sum()
    }

    fn flight_count(&self) -> usize {
        self.flights.lock().len()
    }
}

/// One request's place in a flight. Dropped by the last request of the flight, it removes the
/// flight. Every place is taken and given back under the flights' lock, so the count read there is
/// exact.
struct FlightHolder<'a> {
    flights: &'a Mutex<HashMap<FlightKey, Flight>>,
    key: FlightKey,
    /// `None` only once the place has been given back, while dropping.
    cell: Option<Flight>,
}

impl<'a> FlightHolder<'a> {
    fn join(flights: &'a Mutex<HashMap<FlightKey, Flight>>, key: FlightKey) -> Self {
        let cell = Arc::clone(flights.lock().entry(key.clone()).or_default());
        Self {
            flights,
            key,
            cell: Some(cell),
        }
    }

    /// The flight's outcome, loaded with `load` unless another request is loading it already.
    async fn outcome<F, Fut>(&self, load: F) -> LoadOutcome
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = LoadOutcome>,
    {
        match &self.cell {
            Some(cell) => cell.get_or_init(load).await.clone(),
            None => load().await,
        }
    }
}

impl Drop for FlightHolder<'_> {
    fn drop(&mut self) {
        let mut flights = self.flights.lock();
        let Some(cell) = self.cell.take() else {
            return;
        };
        // The map's reference and this one: no other request is waiting on the flight.
        let last = flights
            .get(&self.key)
            .is_some_and(|held| Arc::ptr_eq(held, &cell) && Arc::strong_count(held) == 2);
        if last {
            flights.remove(&self.key);
        }
        // Given back before the lock is: a reference released after it would let two last
        // requests each count the other and leave the flight, and its outcome, in the map.
        drop(cell);
        drop(flights);
    }
}

/// Read `file`, `len` bytes long when it was opened, under `guard`. The collector refuses past the
/// ceiling, so a file that grows after its length was read is still bounded.
fn read_under(
    file: &mut std::fs::File,
    len: u64,
    guard: &BodyGuard,
) -> Result<CollectedBody, BodyReadError> {
    use std::io::Read;
    let mut collector = guard.try_start(Some(len))?;
    let mut chunk = vec![0; 64 * 1024];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => collector.push(&chunk[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(BodyReadError::Read(Box::new(e))),
        }
    }
    Ok(collector.finish())
}

/// Parsed CSV data
pub struct CsvData {
    /// Column headers
    headers: Vec<String>,
    /// Data rows in file order: a lookup serves the first matching row, as Mountebank's
    /// `selectRowFromCSV` does (issue #1384).
    rows: Vec<Vec<String>>,
    /// First-column value → index of the first row holding it, so the common first-column
    /// lookup stays O(1).
    first_column: HashMap<String, usize>,
}

impl CsvData {
    /// Read the CSV file at `path` and parse it with [`CsvData::parse`].
    ///
    /// A missing, unreadable, non-UTF-8 or empty file is an error. A file that stops parsing
    /// part-way is not: the rows before the fault are kept and one warning names the file, the
    /// line and the reason (issue #1566).
    pub fn load<P: AsRef<Path>>(path: P, delimiter: &str) -> Result<Self, std::io::Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)?;
        Self::from_text(&text, &path.display().to_string(), delimiter)
    }

    /// [`CsvData::load`] for text already read from the file `name` names.
    fn from_text(text: &str, name: &str, delimiter: &str) -> Result<Self, std::io::Error> {
        if text.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Empty CSV",
            ));
        }
        let (data, error) = Self::parse(text, delimiter);
        if let Some(error) = error {
            tracing::warn!(
                "lookup behavior: CSV data source '{}' stops at line {}: {}; rows from there on \
                 are not read, as in Mountebank",
                name,
                error.line,
                error.kind
            );
        }
        Ok(data)
    }

    /// Parse CSV `text` as Mountebank's csv-parse 7.0.2 does, and index it for lookups. The first
    /// record is the header row. Rows after the first parse error do not exist, which is what
    /// Mountebank's streamed parse observably does; the error is returned for the caller to report.
    ///
    /// Takes text, not a path, so file access stays the caller's (see [`CsvData::load`]).
    pub fn parse(text: &str, delimiter: &str) -> (Self, Option<CsvParseError>) {
        let ParsedCsv { records, error } = parse_csv(text, delimiter);
        let mut records = records.into_iter();
        // Domain-optional: a file whose header row itself fails has no headers, so every lookup
        // misses, as in Mountebank.
        let headers = records.next().unwrap_or_default();
        let rows: Vec<Vec<String>> = records.collect();
        let mut first_column = HashMap::new();
        for (index, values) in rows.iter().enumerate() {
            // Every parsed record holds at least one cell, so `first` is always present.
            if let Some(first) = values.first() {
                first_column.entry(first.clone()).or_insert(index);
            }
        }
        (
            Self {
                headers,
                rows,
                first_column,
            },
            error,
        )
    }

    /// The heap memory this parse holds, approximately: every cell's and header's string
    /// capacity, the vectors holding them, and the first-column index. It grows with the content,
    /// which is what the [`CsvCache`] bound needs of it (issue #1654).
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let strings = |cells: &[String], capacity: usize| {
            capacity * size_of::<String>() + cells.iter().map(String::capacity).sum::<usize>()
        };
        strings(&self.headers, self.headers.capacity())
            + self.rows.capacity() * size_of::<Vec<String>>()
            + self
                .rows
                .iter()
                .map(|row| strings(row, row.capacity()))
                .sum::<usize>()
            + self.first_column.capacity() * (size_of::<String>() + size_of::<usize>())
            + self
                .first_column
                .keys()
                .map(String::capacity)
                .sum::<usize>()
    }

    /// Look up the first row, in file order, whose `key_column` cell equals `key`, and return its
    /// cells as `[column_name]` token replacements. Empty when no row matches or `key_column` is
    /// not a header.
    pub fn lookup(&self, key: &str, key_column: &str) -> HashMap<String, String> {
        let Some(key_idx) = self.headers.iter().position(|h| h == key_column) else {
            return HashMap::new();
        };
        let row = if key_idx == 0 {
            self.first_column.get(key).map(|&i| &self.rows[i])
        } else {
            self.rows
                .iter()
                .find(|values| values.get(key_idx).is_some_and(|v| v == key))
        };
        let Some(values) = row else {
            return HashMap::new();
        };
        self.headers
            .iter()
            .zip(values)
            .map(|(header, value)| (format!("[{header}]"), value.clone()))
            .collect()
    }
}

/// Why a CSV file stopped being read, named after csv-parse's error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsvErrorKind {
    /// `CSV_QUOTE_NOT_CLOSED`: a quoted cell runs to the end of the file.
    QuoteNotClosed,
    /// `CSV_INVALID_CLOSING_QUOTE`: something other than a delimiter, a record end or the end of
    /// the file follows a closing quote.
    InvalidClosingQuote,
    /// `INVALID_OPENING_QUOTE`: a `"` inside an unquoted cell.
    InvalidOpeningQuote,
    /// `CSV_RECORD_INCONSISTENT_FIELDS_LENGTH`: a record whose width differs from the first's.
    InconsistentFieldsLength,
}

impl fmt::Display for CsvErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::QuoteNotClosed => "quoted field not terminated",
            Self::InvalidClosingQuote => "text after a closing quote",
            Self::InvalidOpeningQuote => "quote inside an unquoted field",
            Self::InconsistentFieldsLength => "record width differs from the header row",
        })
    }
}

/// A CSV parse error and the 1-based line it is reported at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvParseError {
    /// What went wrong.
    pub kind: CsvErrorKind,
    /// The line of the fault: an unclosed quote's opening line, a ragged record's first line.
    pub line: usize,
}

/// The records parsed before the first error, and that error.
struct ParsedCsv {
    records: Vec<Vec<String>>,
    error: Option<CsvParseError>,
}

/// Parse `text` into records with csv-parse 7.0.2's default rules (issue #1566):
///
/// 1. A field starting with `"` is quoted: `""` inside is one `"`, and the delimiter and line
///    breaks are literal. The end of the file inside the quotes is an error.
/// 2. After a closing quote only the delimiter, the record end or the end of the file may follow.
/// 3. A `"` inside an unquoted field is an error.
/// 4. The record end is the first of `\r\n`, `\n` or `\r` seen outside quotes; after that only
///    that sequence ends a record.
/// 5. Nothing is trimmed.
/// 6. The first record fixes the width; a record of another width is an error. A blank line is
///    a record of one empty cell.
/// 7. A record end at the end of the file does not start another record.
///
/// Parsing stops at the first error, keeping the records before it.
fn parse_csv(text: &str, delimiter: &str) -> ParsedCsv {
    // Mountebank passes `csvConfig.delimiter || ','`, so an empty delimiter is a comma.
    let delimiter = if delimiter.is_empty() { "," } else { delimiter };
    let mut parser = CsvParser {
        text,
        delimiter: delimiter.as_bytes(),
        pos: 0,
        record_end: None,
    };
    let mut records = Vec::new();
    let mut width = None;
    while parser.pos < text.len() {
        let start = parser.pos;
        let fail = |kind, offset| CsvParseError {
            kind,
            line: line_at(text, offset),
        };
        let record = match parser.record() {
            Ok(record) => record,
            Err((kind, offset)) => {
                return ParsedCsv {
                    records,
                    error: Some(fail(kind, offset)),
                };
            }
        };
        match width {
            None => width = Some(record.len()),
            Some(width) if width != record.len() => {
                return ParsedCsv {
                    records,
                    error: Some(fail(CsvErrorKind::InconsistentFieldsLength, start)),
                };
            }
            Some(_) => {}
        }
        records.push(record);
    }
    ParsedCsv {
        records,
        error: None,
    }
}

/// The 1-based line `offset` is on, counting `\r\n`, `\n` and a lone `\r` as one break each.
fn line_at(text: &str, offset: usize) -> usize {
    let bytes = &text.as_bytes()[..offset];
    let breaks = bytes
        .iter()
        .enumerate()
        .filter(|&(i, &b)| b == b'\n' || (b == b'\r' && bytes.get(i + 1) != Some(&b'\n')))
        .count();
    breaks + 1
}

/// Cursor over the CSV text. It scans bytes: the quote, the line breaks and the start of a
/// (valid UTF-8) delimiter all sit on character boundaries, so every slice it takes is one.
struct CsvParser<'a> {
    text: &'a str,
    delimiter: &'a [u8],
    pos: usize,
    /// The record-end sequence, fixed by the first one seen outside quotes.
    record_end: Option<&'static [u8]>,
}

impl CsvParser<'_> {
    fn rest(&self) -> &[u8] {
        &self.text.as_bytes()[self.pos..]
    }

    fn at_delimiter(&self) -> bool {
        self.rest().starts_with(self.delimiter)
    }

    /// The length of the record end at the cursor, or 0. The first line break seen fixes the
    /// sequence for the rest of the file.
    fn record_end_len(&mut self) -> usize {
        let rest = self.rest();
        let end: &'static [u8] = match self.record_end {
            Some(end) => end,
            None if rest.starts_with(b"\r\n") => b"\r\n",
            None if rest.starts_with(b"\n") => b"\n",
            None if rest.starts_with(b"\r") => b"\r",
            None => return 0,
        };
        if rest.starts_with(end) {
            self.record_end = Some(end);
            end.len()
        } else {
            0
        }
    }

    /// Read one record, consuming its record end. The error carries the offset it is reported at.
    fn record(&mut self) -> Result<Vec<String>, (CsvErrorKind, usize)> {
        let mut fields = Vec::new();
        loop {
            let quoted = self.rest().first() == Some(&b'"');
            fields.push(if quoted {
                self.quoted()?
            } else {
                self.unquoted()?
            });
            if self.pos >= self.text.len() {
                return Ok(fields);
            }
            if self.at_delimiter() {
                self.pos += self.delimiter.len();
                continue;
            }
            let end = self.record_end_len();
            if end > 0 {
                self.pos += end;
                return Ok(fields);
            }
            // An unquoted field only stops at the delimiter, a record end or the end of the file,
            // so this is text after a closing quote.
            return Err((CsvErrorKind::InvalidClosingQuote, self.pos));
        }
    }

    /// Read a quoted field; the cursor is on its opening quote.
    fn quoted(&mut self) -> Result<String, (CsvErrorKind, usize)> {
        let open = self.pos;
        let bytes = self.text.as_bytes();
        self.pos += 1;
        let mut field = String::new();
        let mut segment = self.pos;
        loop {
            match bytes.get(self.pos) {
                None => return Err((CsvErrorKind::QuoteNotClosed, open)),
                Some(b'"') if bytes.get(self.pos + 1) == Some(&b'"') => {
                    field.push_str(&self.text[segment..=self.pos]);
                    self.pos += 2;
                    segment = self.pos;
                }
                Some(b'"') => {
                    field.push_str(&self.text[segment..self.pos]);
                    self.pos += 1;
                    return Ok(field);
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    /// Read an unquoted field, up to the delimiter, a record end or the end of the file.
    fn unquoted(&mut self) -> Result<String, (CsvErrorKind, usize)> {
        let start = self.pos;
        while let Some(&byte) = self.text.as_bytes().get(self.pos) {
            if self.at_delimiter() {
                break;
            }
            if matches!(byte, b'\r' | b'\n') && self.record_end_len() > 0 {
                break;
            }
            if byte == b'"' {
                return Err((CsvErrorKind::InvalidOpeningQuote, self.pos));
            }
            self.pos += 1;
        }
        Ok(self.text[start..self.pos].to_string())
    }
}

/// Apply lookup behaviors to response body. Treats `body` and `headers` as authored text; the
/// serve path uses [`apply_lookup_spliced`] so text an earlier pass inserted is not re-scanned.
///
/// Files are read under a default [`BodyGuard`] of their own, which nothing else draws on.
pub fn apply_lookup_behaviors(
    body: &str,
    headers: &mut crate::imposter::Headers,
    behaviors: &[LookupBehavior],
    request: &RequestContext,
    csv_cache: &CsvCache,
    access: &ScriptFileAccess,
    stub: crate::imposter::headers::StubRef<'_>,
) -> String {
    let mut spliced_body = Spliced::authored(body.to_string());
    let mut spliced_headers = authored_headers(std::mem::take(headers));
    if let Err(e) = apply_lookup_spliced(
        &mut spliced_body,
        &mut spliced_headers,
        &mut None,
        behaviors,
        request,
        csv_cache,
        access,
        &BodyGuard::default(),
        stub,
    ) {
        // A fresh default guard's budget is larger than its ceiling and held by nothing else, so
        // a file is refused as too large before the budget can run out. Logged should it happen.
        tracing::error!("lookup behavior: {e}; the response is served without this lookup");
    }
    *headers = plain_headers(spliced_headers);
    spliced_body.into_text()
}

/// Apply lookup behaviors, replacing only tokens that are not wholly inside text an earlier pass
/// inserted (issue #1203): request text a `${request.*}`, `{{ }}` or `copy` pass spliced in is
/// never read as a lookup token, so a client cannot choose which column of the row is served.
///
/// Each cell is recorded as inserted too, so a cell holding another column's token is served as
/// written — regardless of the order the row's columns are visited in.
///
/// `status` is a `statusCode` string (issue #1558), scanned like a header value, as Mountebank's
/// `globalObjectReplace` replaces tokens in every string field of the response.
///
/// `access` is the imposter's: it decides which CSV files may be read (issue #1617). `guard` is
/// its body limits, under which a file is read (issue #1654).
///
/// Reads a file it has not cached on the calling thread; the serve path instead loads it off the
/// async worker, with [`lookup_key`], [`CsvCache::get_fresh`], [`CsvCache::load`] and
/// [`apply_lookup_row`].
///
/// # Errors
/// [`LookupLoadError::BudgetExhausted`] when the body memory budget cannot hold a file now.
#[allow(clippy::too_many_arguments)] // each is a distinct input of one pass; a struct adds nothing
pub(crate) fn apply_lookup_spliced(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    status: &mut Option<Spliced>,
    behaviors: &[LookupBehavior],
    request: &RequestContext,
    csv_cache: &CsvCache,
    access: &ScriptFileAccess,
    guard: &BodyGuard,
    stub: crate::imposter::headers::StubRef<'_>,
) -> Result<(), LookupLoadError> {
    for behavior in behaviors {
        let Some(key) = lookup_key(behavior, request) else {
            continue;
        };
        let csv = &behavior.from_data_source.csv;
        if let Some(csv_data) = csv_cache.get_or_load(&csv.path, &csv.delimiter, access, guard)? {
            apply_lookup_row(body, headers, status, behavior, &key, &csv_data, stub);
        }
    }
    Ok(())
}

/// The key `behavior` looks up for `request`, or `None` when there is none to look up.
///
/// Mountebank's `lookup` catches a key selector that fails (issue #1585), logs it at error level
/// and leaves the response as it is, so the token stays as authored.
pub(crate) fn lookup_key(behavior: &LookupBehavior, request: &RequestContext) -> Option<String> {
    match behavior.key.extract(request) {
        Ok(key) => key,
        Err(e) => {
            tracing::error!("lookup key: {e}; the response is served without this lookup");
            None
        }
    }
}

/// Replace `behavior`'s tokens with the cells of the row `key` selects in `csv_data`. See
/// [`apply_lookup_spliced`] for which tokens are replaced.
pub(crate) fn apply_lookup_row(
    body: &mut Spliced,
    headers: &mut SplicedHeaders,
    status: &mut Option<Spliced>,
    behavior: &LookupBehavior,
    key: &str,
    csv_data: &CsvData,
    stub: crate::imposter::headers::StubRef<'_>,
) {
    let replacements = csv_data.lookup(key, &behavior.from_data_source.csv.key_column);

    // Mountebank (behaviors.js replaceObjectValuesIn) accepts the column as `["col"]`, `['col']`
    // or `[col]`; `lookup` keys each cell by the unquoted form.
    for (column_token, value) in replacements {
        let column = column_token
            .strip_prefix('[')
            .and_then(|c| c.strip_suffix(']'))
            .unwrap_or(&column_token);
        let into = &behavior.into;
        for full_token in [
            format!("{into}[\"{column}\"]"),
            format!("{into}['{column}']"),
            format!("{into}[{column}]"),
        ] {
            replace_response_token(body, headers, status, &full_token, &value, stub);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture identity for tests that only care about the repair itself, not which stub
    /// triggered it — the gate test below (issue #1075) asserts on deliberately chosen values.
    const FIXTURE_STUB: crate::imposter::headers::StubRef<'static> =
        crate::imposter::headers::StubRef {
            port: 0,
            index: 0,
            id: None,
        };

    /// Issue #1067: the client picks the row, so a CSV cell holding a byte a header value cannot
    /// carry is a client-selectable 500. Repair it instead.
    #[test]
    fn lookup_repairs_a_header_value_it_rewrote() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = crate::util::OrderedMap::default();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ",".to_string(),
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert("X-Echo".to_string(), vec!["n=${row}[name]".to_string()]);

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Echo"],
            vec!["n=World".to_string()],
            "the ESC in the CSV cell is removed; without the repair this value is unrepresentable"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Issue #1075: the repair `lookup` triggers must name the stub too.
    #[test]
    fn lookup_repair_warning_names_the_stub() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1075_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = crate::util::OrderedMap::default();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ",".to_string(),
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert("X-Echo".to_string(), vec!["n=${row}[name]".to_string()]);

        // See the note in `copy.rs`'s sibling test for why this is `captured_logs` and not
        // `tracing_test::traced_test`.
        let events = crate::test_support::captured_logs(|| {
            apply_lookup_behaviors(
                "",
                &mut headers,
                &behaviors,
                &request,
                &CsvCache::default(),
                &ScriptFileAccess::trusted_cwd(),
                crate::imposter::headers::StubRef {
                    port: 4646,
                    index: 5,
                    id: None,
                },
            );
        });

        assert_eq!(headers["X-Echo"], vec!["n=World".to_string()]);
        assert_eq!(events.len(), 1, "one repair, one warning");
        assert_eq!(events[0].field("port"), Some("4646"));
        assert_eq!(events[0].field("stub"), Some("5"));
        assert_eq!(
            events[0].field("stub_id"),
            Some(""),
            "no id -> empty, not absent"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Only the substituted cell is repaired; a literal control character the author typed beside
    /// the token survives so the response still fails loudly (issue #1067).
    #[test]
    fn lookup_repairs_the_cell_but_not_a_literal_control_char_beside_the_token() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067c_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,Wor\u{1b}ld\n").expect("write csv");

        let mut query = crate::util::OrderedMap::default();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ",".to_string(),
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert(
            "X-Echo".to_string(),
            vec!["bad\nvalue=${row}[name]".to_string()],
        );

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Echo"],
            vec!["bad\nvalue=World".to_string()],
            "the ESC from the CSV cell is gone; the author's literal newline is not"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The boundary (issue #1067): a header value this behavior never rewrote is a literal from
    /// the config and must reach the builder unrepaired.
    #[test]
    fn lookup_leaves_a_header_value_without_the_token_untouched() {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1067b_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,World\n").expect("write csv");

        let mut query = crate::util::OrderedMap::default();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ",".to_string(),
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert("X-Literal".to_string(), vec!["bad\nvalue".to_string()]);

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["X-Literal"],
            vec!["bad\nvalue".to_string()],
            "a literal config value must reach the builder unrepaired and fail loudly"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn lookup_preserves_multi_value_headers_and_substitutes_each() {
        let path = std::env::temp_dir().join(format!("rift_lookup_272_{}.csv", std::process::id()));
        std::fs::write(&path, "id,name\nhi,World\n").expect("write csv");

        let mut query = crate::util::OrderedMap::default();
        query.insert("q".to_string(), vec!["hi".to_string()]);
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/x".to_string(),
            query,
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };

        let behaviors = vec![LookupBehavior {
            key: LookupKey {
                from: {
                    let mut map = HashMap::new();
                    map.insert("query".to_string(), "q".to_string());
                    CopySource::Nested(map)
                },
                extraction: ExtractionMethod::Regex {
                    selector: ".*".to_string(),
                    options: None,
                },
                index: None,
            },
            from_data_source: DataSource {
                csv: CsvDataSource {
                    path: path.to_string_lossy().into_owned(),
                    key_column: "id".to_string(),
                    delimiter: ",".to_string(),
                },
            },
            into: "${row}".to_string(),
        }];

        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a=1".to_string(), "n=${row}[name]".to_string()],
        );

        apply_lookup_behaviors(
            "",
            &mut headers,
            &behaviors,
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );

        assert_eq!(
            headers["Set-Cookie"],
            vec!["a=1".to_string(), "n=World".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `_rift.dataset` block survives a parse/serialize round trip.
    ///
    /// This is the whole contract of the type: the cluster stores what the operator wrote, and a
    /// field that silently vanishes on parse means the binding cannot be stored at all. Asserted
    /// on the *parsed* values rather than by string equality, so a reordering of the struct's
    /// fields does not read as a regression.
    #[test]
    fn a_dataset_binding_round_trips() {
        let json = r#"{
            "name": "customers",
            "version": 3,
            "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
            "keyColumn": "customer_id",
            "into": "${row}",
            "digest": "abc123"
        }"#;

        let parsed: DatasetBinding = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.name, "customers");
        assert_eq!(parsed.version, Some(3));
        assert_eq!(parsed.key_column, "customer_id");
        assert_eq!(parsed.into, "${row}");
        assert_eq!(parsed.digest.as_deref(), Some("abc123"));

        let reparsed: DatasetBinding =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("serializes"))
                .expect("re-parses");
        assert_eq!(reparsed.name, parsed.name);
        assert_eq!(reparsed.version, parsed.version);
        assert_eq!(reparsed.key_column, parsed.key_column);
        assert_eq!(reparsed.into, parsed.into);
        assert_eq!(reparsed.digest, parsed.digest);
    }

    /// `version` and `digest` are optional going in, and absent going out.
    ///
    /// An unbound block is what an operator writes ("latest at bind time"); emitting
    /// `"version": null` would make an unresolved binding indistinguishable from one pinned to a
    /// version that failed to parse.
    #[test]
    fn an_unpinned_dataset_binding_omits_version_and_digest() {
        let json = r#"{
            "name": "customers",
            "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
            "keyColumn": "id",
            "into": "${row}"
        }"#;

        let parsed: DatasetBinding = serde_json::from_str(json).expect("parses");
        assert_eq!(parsed.version, None);
        assert_eq!(parsed.digest, None);

        let out = serde_json::to_string(&parsed).expect("serializes");
        assert!(
            !out.contains("version"),
            "unpinned block must omit version: {out}"
        );
        assert!(
            !out.contains("digest"),
            "unpinned block must omit digest: {out}"
        );
    }

    // Issue #1240: Mountebank's lookup `key` takes an `index` beside `from`/`using`, choosing which
    // element of the selector's match array keys the row (`lookupRow`: `keyValues[index]`). For a
    // regex that array is `RegExp.exec`'s: the whole match at 0, then each capture group. Rift
    // ignored the field and always keyed on the first group.

    /// Runs one lookup parsed from `key` against `path`/`body` and returns the served body.
    fn lookup_with_key(
        tag: &str,
        key: serde_json::Value,
        path: &str,
        body: Option<&str>,
    ) -> String {
        let csv =
            std::env::temp_dir().join(format!("rift_lookup_1240_{tag}_{}.csv", std::process::id()));
        std::fs::write(
            &csv,
            "id,name\nab,first\ncd,second\n/ab-cd,whole\nb2,second-book\nundefined,from-undefined\n",
        )
        .expect("write csv");
        let behavior: LookupBehavior = serde_json::from_value(serde_json::json!({
            "key": key,
            "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
            "into": "${row}"
        }))
        .expect("the lookup parses");
        let request = RequestContext {
            method: "GET".to_string(),
            path: path.to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: body.map(str::to_string),
            ..Default::default()
        };
        let served = apply_lookup_behaviors(
            "${row}[name]",
            &mut crate::imposter::Headers::new(),
            &[behavior],
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );
        let _ = std::fs::remove_file(&csv);
        served
    }

    fn regex_key(index: Option<u64>) -> serde_json::Value {
        let mut key = serde_json::json!({
            "from": "path",
            "using": {"method": "regex", "selector": "^/(ab|cd)-(ab|cd)$"}
        });
        if let Some(i) = index {
            key["index"] = serde_json::json!(i);
        }
        key
    }

    #[test]
    fn a_regex_lookup_index_selects_that_capture_group() {
        assert_eq!(
            lookup_with_key("g2", regex_key(Some(2)), "/ab-cd", None),
            "second"
        );
        assert_eq!(
            lookup_with_key("g1", regex_key(Some(1)), "/ab-cd", None),
            "first"
        );
    }

    #[test]
    fn a_regex_lookup_index_zero_is_the_whole_match() {
        assert_eq!(
            lookup_with_key("g0", regex_key(Some(0)), "/ab-cd", None),
            "whole"
        );
    }

    /// Without `index` the key stays what Rift has always used (the first capture group), so no
    /// existing config changes meaning.
    #[test]
    fn a_lookup_without_index_keeps_the_first_capture_group() {
        assert_eq!(
            lookup_with_key("none", regex_key(None), "/ab-cd", None),
            "first"
        );
    }

    /// An index past the match array keys nothing, as `keyValues[index]` is `undefined` in
    /// Mountebank: no row is found and the token is left for the author to see.
    #[test]
    fn a_lookup_index_past_the_matches_finds_no_row() {
        assert_eq!(
            lookup_with_key("oob", regex_key(Some(3)), "/ab-cd", None),
            "${row}[name]"
        );
    }

    #[test]
    fn a_jsonpath_lookup_index_selects_that_match() {
        let key = serde_json::json!({
            "from": "body",
            "using": {"method": "jsonpath", "selector": "$.books[*].id"},
            "index": 1
        });
        assert_eq!(
            lookup_with_key(
                "jp",
                key,
                "/",
                Some(r#"{"books":[{"id":"ab"},{"id":"b2"}]}"#)
            ),
            "second-book"
        );
    }

    /// Issue #1582: Mountebank has no rule for `key.index` and reads `keyValues[index || 0]`, so a
    /// value that is not a match position loads and finds no row.
    #[test]
    fn a_lookup_index_that_is_no_position_loads_and_finds_no_row() {
        for index in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("x"),
        ] {
            let mut key = regex_key(None);
            key["index"] = index.clone();
            let parsed: LookupKey = serde_json::from_value(key.clone()).expect("admitted");
            assert_eq!(
                serde_json::to_value(&parsed).expect("print")["index"],
                index,
                "printed as written"
            );
            assert_eq!(
                lookup_with_key("nopos", key, "/ab-cd", None),
                "${row}[name]",
                "{index}"
            );
        }
    }

    #[test]
    fn a_lookup_index_resolves_as_a_javascript_property_of_the_match_array() {
        use serde_json::json;
        for (index, position) in [
            (json!(0), Some(0)),
            (json!(2), Some(2)),
            (json!(1.0), Some(1)),
            (json!(-0.0), Some(0)),
            (json!(""), Some(0)),
            (json!(false), Some(0)),
            (json!(null), Some(0)),
            (json!("0"), Some(0)),
            (json!("12"), Some(12)),
            (json!("01"), None),
            (json!(" 1"), None),
            (json!("1e0"), None),
            (json!("99999999999999999999999"), None),
            (json!(-1), None),
            (json!(1.5), None),
            (json!(true), None),
            (json!([1]), None),
            (json!({}), None),
        ] {
            assert_eq!(LookupIndex(index.clone()).position(), position, "{index}");
        }
    }

    /// `_rift.dataset.key` is a [`LookupKey`] and must hand back the `index` the operator wrote.
    #[test]
    fn a_dataset_binding_keeps_the_key_index() {
        let parsed: DatasetBinding = serde_json::from_value(serde_json::json!({
            "name": "d",
            "key": {"from": "path", "using": {"method": "regex", "selector": "/(.*)$"}, "index": 1},
            "keyColumn": "id",
            "into": "${row}"
        }))
        .expect("parses");
        let out = serde_json::to_value(&parsed).expect("serializes");
        assert_eq!(out["key"]["index"], serde_json::json!(1), "{out}");

        let unindexed: DatasetBinding = serde_json::from_value(serde_json::json!({
            "name": "d",
            "key": {"from": "path", "using": {"method": "regex", "selector": "/(.*)$"}},
            "keyColumn": "id",
            "into": "${row}"
        }))
        .expect("parses");
        let out = serde_json::to_value(&unindexed).expect("serializes");
        assert!(
            out["key"].get("index").is_none(),
            "absent stays absent: {out}"
        );
    }

    /// Writes `contents` to a per-process temp CSV named after `tag` and loads it.
    fn csv_data(tag: &str, contents: &str) -> CsvData {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1384_{tag}_{}.csv", std::process::id()));
        std::fs::write(&path, contents).expect("write csv");
        let data = CsvData::load(&path, ",").expect("load csv");
        let _ = std::fs::remove_file(&path);
        data
    }

    /// Issue #1384: Mountebank's `selectRowFromCSV` resolves with the first row whose key
    /// column matches, in file order. Rift used to key rows by first cell in a map, so a
    /// repeated key served the last row.
    #[test]
    fn a_repeated_key_serves_the_first_row_in_file_order() {
        let data = csv_data("repeat", "id,name\n1,alice\n1,carol\n");
        assert_eq!(data.lookup("1", "id")["[name]"], "alice");
    }

    /// Issue #1384: a key column other than the first scans rows in file order and serves the
    /// first match, not whichever row a randomly seeded map yields first.
    #[test]
    fn a_non_first_key_column_serves_the_first_matching_row() {
        let data = csv_data(
            "column",
            "id,name,city\n1,alice,paris\n2,bob,rome\n3,dave,rome\n",
        );
        let rome = data.lookup("rome", "city");
        assert_eq!(rome["[name]"], "bob");
        assert_eq!(rome["[id]"], "2");
        assert_eq!(data.lookup("bob", "name")["[city]"], "rome");
        assert!(data.lookup("oslo", "city").is_empty());
    }

    /// Issue #1617 (folded in from #1566): the cache is keyed by path and delimiter, so a second
    /// delimiter over the same file gets its own parse instead of the first one's.
    #[test]
    fn the_cache_keeps_one_parse_per_delimiter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mixed.csv");
        std::fs::write(&path, "k;v\n1;semi\n").expect("write csv");
        let path = path.to_string_lossy().into_owned();
        let cache = CsvCache::default();
        let access = ScriptFileAccess::trusted_cwd();
        let comma = load(&cache, &path, ",", &access).expect("comma");
        assert_eq!(comma.lookup("1;semi", "k;v")["[k;v]"], "1;semi");
        let semi = load(&cache, &path, ";", &access).expect("semi");
        assert_eq!(semi.lookup("1", "k")["[v]"], "semi");
    }

    /// A path the access refuses is never read, cached or not (issue #1617).
    #[test]
    fn the_cache_reads_nothing_the_access_refuses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("data.csv");
        std::fs::write(&path, "id,name\n1,x\n").expect("write csv");
        let path = path.to_string_lossy().into_owned();
        let cache = CsvCache::default();
        assert!(load(&cache, &path, ",", &ScriptFileAccess::trusted_cwd()).is_some());
        assert!(load(&cache, &path, ",", &ScriptFileAccess::Refused).is_none());
        let elsewhere = tempfile::tempdir().expect("tempdir");
        assert!(
            load(
                &cache,
                &path,
                ",",
                &ScriptFileAccess::Confined(elsewhere.path().to_path_buf())
            )
            .is_none(),
            "the cached parse is not served to an access that may not read the file"
        );
    }

    /// `clear` drops cached files, so the next lookup reads the file again.
    #[test]
    fn clear_drops_the_cached_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("clear.csv");
        std::fs::write(&path, "id,name\n1,old\n").expect("write csv");
        let path = path.to_string_lossy().into_owned();
        let cache = CsvCache::default();
        assert!(load(&cache, &path, ",", &ScriptFileAccess::trusted_cwd()).is_some());
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.cached_bytes(), 0);
    }

    /// The default-guard `get_or_load` a test reads a file with: the parse, or `None` when it is
    /// not loaded.
    fn load(
        cache: &CsvCache,
        path: &str,
        delimiter: &str,
        access: &ScriptFileAccess,
    ) -> Option<Arc<CsvData>> {
        cache
            .get_or_load(path, delimiter, access, &BodyGuard::default())
            .expect("the default budget holds a test file")
            .map(|handle| Arc::clone(handle.data()))
    }

    fn guard(max: usize, budget: Option<usize>) -> BodyGuard {
        BodyGuard::new(
            crate::imposter::body_limits::BodyLimits::new(max, budget).expect("valid limits"),
        )
    }

    /// A CSV file of at least `len` bytes whose row `1` has `name` = `marker`.
    fn padded_csv(dir: &Path, file: &str, marker: &str, len: usize) -> String {
        let mut text = format!("id,name\n1,{marker}\n");
        // Long cells, so a parse is a small multiple of the file and several fit a bound.
        let padding = format!("9,{}\n", "p".repeat(88));
        while text.len() < len {
            text.push_str(&padding);
        }
        let path = dir.join(file);
        std::fs::write(&path, text).expect("write csv");
        path.to_string_lossy().into_owned()
    }

    /// Issue #1654: every delimiter is a parse of its own, so before the bound an admin client
    /// could grow the cache without limit by varying it. The cache now stays within the ceiling.
    #[test]
    fn the_cache_stays_within_its_bound_across_delimiters() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "big.csv", "x", 10 * 1024);
        let cache = CsvCache::default();
        let guard = guard(64 * 1024, None);
        let access = ScriptFileAccess::trusted_cwd();
        for i in 0..100 {
            let delimiter = format!("|{i}|");
            let data = cache
                .get_or_load(&path, &delimiter, &access, &guard)
                .expect("no budget to exhaust")
                .expect("loads");
            // With a delimiter that never occurs, each line is one cell under the header `id,name`.
            assert_eq!(data.lookup("1,x", "id,name")["[id,name]"], "1,x");
            assert!(!cache.is_empty(), "a parse within the bound is cached");
            assert!(
                cache.cached_bytes() <= 64 * 1024,
                "after {} delimiters the cache holds {} bytes",
                i + 1,
                cache.cached_bytes()
            );
        }
        assert!(cache.len() < 100, "older parses were evicted");
    }

    /// The least recently used file is the one evicted.
    #[test]
    fn the_least_recently_used_file_is_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = padded_csv(dir.path(), "a.csv", "a", 2048);
        let b = padded_csv(dir.path(), "b.csv", "b", 2048);
        let c = padded_csv(dir.path(), "c.csv", "c", 2048);
        let access = ScriptFileAccess::trusted_cwd();
        let one = {
            let cache = CsvCache::default();
            load(&cache, &a, ",", &access).expect("loads");
            cache.cached_bytes()
        };
        // Room for two files, not three.
        let guard = guard(2 * one + one / 2, None);
        let cache = CsvCache::default();
        for path in [&a, &b, &a, &c] {
            cache
                .get_or_load(path, ",", &access, &guard)
                .expect("no budget")
                .expect("loads");
        }
        let cached = |path: &str| cache.get_fresh(path, ",", &access).is_some();
        assert!(cached(&a), "used after b, so kept");
        assert!(!cached(&b), "the least recently used, so evicted");
        assert!(cached(&c));
    }

    #[test]
    fn an_entry_larger_than_the_bound_is_served_but_not_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The file fits the 4 KiB ceiling; its parse, with every cell a `String`, does not.
        let path = padded_csv(dir.path(), "wide.csv", "w", 3 * 1024);
        let cache = CsvCache::default();
        let data = cache
            .get_or_load(
                &path,
                ",",
                &ScriptFileAccess::trusted_cwd(),
                &guard(4096, None),
            )
            .expect("no budget")
            .expect("served");
        assert!(
            data.heap_bytes() > 4096,
            "the fixture's parse is over the bound"
        );
        assert_eq!(data.lookup("1", "id")["[name]"], "w");
        assert!(cache.is_empty(), "not cached");
    }

    #[test]
    #[tracing_test::traced_test]
    fn a_file_over_the_ceiling_is_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "big.csv", "x", 4096);
        let cache = CsvCache::default();
        let loaded = cache.get_or_load(
            &path,
            ",",
            &ScriptFileAccess::trusted_cwd(),
            &guard(1024, None),
        );
        assert_eq!(loaded.map(|data| data.is_some()), Ok(false));
        assert!(cache.is_empty());
        assert!(logs_contain("--max-body-size (1024 bytes)"));
    }

    #[tokio::test]
    async fn a_load_the_budget_cannot_hold_is_budget_exhausted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 4096);
        let cache = CsvCache::default();
        let access = ScriptFileAccess::trusted_cwd();
        let guard = guard(64 * 1024, Some(64 * 1024));
        let held = guard.start(Some(62 * 1024)).await.expect("62 KiB fits");
        assert_eq!(
            cache.get_or_load(&path, ",", &access, &guard).map(|_| ()),
            Err(LookupLoadError::BudgetExhausted { path: path.clone() })
        );
        assert!(cache.is_empty());
        drop(held);
        let data = cache
            .get_or_load(&path, ",", &access, &guard)
            .expect("the budget is free again")
            .expect("loads");
        assert_eq!(data.lookup("1", "id")["[name]"], "x");
    }

    #[tokio::test]
    async fn the_reservation_is_released_after_a_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 4096);
        let guard = guard(64 * 1024, Some(64 * 1024));
        CsvCache::default()
            .load(&path, ",", &ScriptFileAccess::trusted_cwd(), &guard)
            .expect("fits")
            .expect("loads");
        guard
            .start_without_waiting(Some(64 * 1024))
            .await
            .expect("the whole budget is free again");
    }

    /// Review of #1654 (B1): an entry is charged its key and a fixed overhead, so a file whose
    /// header fails, an empty parse, still counts and cannot fill memory one delimiter at a time.
    #[test]
    fn an_empty_parse_is_still_charged_and_evicted() {
        let dir = tempfile::tempdir().expect("t");
        let p = dir.path().join("bad.csv");
        std::fs::write(&p, "\"unterminated\n").expect("w");
        let p = p.to_string_lossy().into_owned();
        let cache = CsvCache::default();
        let g = guard(64 * 1024, None);
        for i in 0..10_000 {
            let _ = cache.get_or_load(&p, &format!("|{i}|"), &ScriptFileAccess::trusted_cwd(), &g);
        }
        assert!(
            cache.cached_bytes() <= 64 * 1024 && cache.len() * ENTRY_OVERHEAD <= 64 * 1024,
            "{} entries, {} bytes",
            cache.len(),
            cache.cached_bytes()
        );
    }

    /// An empty delimiter is a comma, so both name one parse (review of #1654, N3).
    #[test]
    fn an_empty_delimiter_shares_the_comma_parse() {
        let dir = tempfile::tempdir().expect("t");
        let p = padded_csv(dir.path(), "d.csv", "x", 10);
        let cache = CsvCache::default();
        let a = ScriptFileAccess::trusted_cwd();
        load(&cache, &p, "", &a).expect("e");
        load(&cache, &p, ",", &a).expect("c");
        assert_eq!(cache.len(), 1, "an empty delimiter is a comma");
    }

    /// A FIFO under a trusted access is refused without blocking (review of #1654, N1).
    #[cfg(unix)]
    #[test]
    fn a_fifo_under_a_trusted_access_is_refused_without_blocking() {
        let dir = tempfile::tempdir().expect("t");
        let p = dir.path().join("fifo.csv");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&p)
                .status()
                .expect("mkfifo")
                .success()
        );
        let p = p.to_string_lossy().into_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = CsvCache::default()
                .get_or_load(
                    &p,
                    ",",
                    &ScriptFileAccess::trusted_cwd(),
                    &BodyGuard::default(),
                )
                .map(|d| d.is_some());
            let _ = tx.send(r);
        });
        let r = rx.recv_timeout(std::time::Duration::from_secs(3));
        assert_eq!(
            r.ok(),
            Some(Ok(false)),
            "refused at once, not blocked on or read"
        );
    }

    /// A parse too large to cache holds its share of the budget until its handle is dropped, so
    /// the budget bounds the uncached parses in flight (review of #1654, S2).
    #[tokio::test]
    async fn an_uncached_parse_holds_the_budget_until_served() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "wide.csv", "w", 3 * 1024);
        // A 4 KiB ceiling: the file fits it, its parse does not fit the cache.
        let g = guard(4096, Some(64 * 1024));
        // How many 4 KiB bodies the budget admits at once.
        let admitted = || async {
            let mut held = Vec::new();
            while let Ok(body) = g.start_without_waiting(Some(4096)).await {
                held.push(body);
            }
            held.len()
        };
        assert_eq!(admitted().await, 16);
        let cache = CsvCache::default();
        let served = cache
            .get_or_load(&path, ",", &ScriptFileAccess::trusted_cwd(), &g)
            .expect("the budget holds it")
            .expect("served");
        assert!(
            served.heap_bytes() > 4096,
            "the fixture's parse is over the bound"
        );
        assert!(cache.is_empty());
        assert!(
            admitted().await < 16,
            "the uncached parse holds its share while it is served"
        );
        drop(served);
        assert_eq!(admitted().await, 16, "released once served");
    }

    /// A parse larger than the whole budget is refused once per unchanged file: the warning is
    /// not repeated and the file is not read again until it changes.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn a_parse_the_budget_can_never_hold_is_refused_once_per_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "wide.csv", "w", 3 * 1024);
        // Ceiling and budget 4 KiB: the file fits, its parse never can.
        let g = guard(4096, Some(4096));
        let cache = CsvCache::default();
        let access = ScriptFileAccess::trusted_cwd();
        assert_eq!(
            cache
                .get_or_load(&path, ",", &access, &g)
                .map(|d| d.is_some()),
            Ok(false)
        );
        // With the whole budget held, a re-read would be refused as exhausted.
        let _held = g
            .start_without_waiting(Some(4096))
            .await
            .expect("fills the budget");
        assert_eq!(
            cache
                .get_or_load(&path, ",", &access, &g)
                .map(|d| d.is_some()),
            Ok(false),
            "the unchanged file is not read again"
        );
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|l| l.contains("more than --body-memory-budget can hold"))
                .count()
            {
                1 => Ok(()),
                n => Err(format!("warned {n} times")),
            }
        });
    }

    /// A parse too large to cache is reported once, not on every read (review of #1654, S2).
    #[test]
    #[tracing_test::traced_test]
    fn an_uncached_parse_is_warned_about_once() {
        let dir = tempfile::tempdir().expect("t");
        let p = padded_csv(dir.path(), "wide.csv", "w", 3 * 1024);
        let g = guard(4096, None);
        let cache = CsvCache::default();
        for _ in 0..3 {
            cache
                .get_or_load(&p, ",", &ScriptFileAccess::trusted_cwd(), &g)
                .expect("b")
                .expect("s");
        }
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|l| l.contains("is not cached") && l.contains("--max-body-size"))
                .count()
            {
                1 => Ok(()),
                n => Err(format!("warned {n} times")),
            }
        });
    }

    /// Mountebank re-reads the file on every lookup, so an edit is seen on the next request.
    #[test]
    fn an_edited_file_is_read_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edited.csv");
        std::fs::write(&path, "id,name\n1,old\n").expect("write csv");
        let path_str = path.to_string_lossy().into_owned();
        let cache = CsvCache::default();
        let name = || {
            load(&cache, &path_str, ",", &ScriptFileAccess::trusted_cwd())
                .expect("loads")
                .lookup("1", "id")["[name]"]
                .clone()
        };
        assert_eq!(name(), "old");
        std::fs::write(&path, "id,name\n1,newer\n").expect("rewrite csv");
        assert_eq!(name(), "newer");
        assert_eq!(cache.len(), 1, "the stale parse was replaced");
    }

    /// Writes `contents` to a per-process temp CSV named after `tag` and loads it with
    /// `delimiter`, as a `lookup` whose `csv.delimiter` is that string would (issue #1566).
    fn csv_with(tag: &str, contents: &str, delimiter: &str) -> CsvData {
        let path =
            std::env::temp_dir().join(format!("rift_lookup_1566_{tag}_{}.csv", std::process::id()));
        std::fs::write(&path, contents).expect("write csv");
        let data = CsvData::load(&path, delimiter).expect("load csv");
        let _ = std::fs::remove_file(&path);
        data
    }

    /// Issue #1566: a quoted cell keeps the delimiter it holds, as csv-parse reads it.
    #[test]
    fn a_quoted_cell_keeps_the_delimiter_it_holds() {
        let data = csv_with(
            "quoted_delim",
            "name,location\n\"The Price Is Right\",\"Darrington, Washington\"\n",
            ",",
        );
        let row = data.lookup("The Price Is Right", "name");
        assert_eq!(row["[location]"], "Darrington, Washington");
    }

    /// Issue #1566: `""` inside a quoted cell is one literal `"`.
    #[test]
    fn a_doubled_quote_in_a_quoted_cell_is_one_quote() {
        let data = csv_with("dq", "id,text\n1,\"He said \"\"hi\"\"\"\n", ",");
        assert_eq!(data.lookup("1", "id")["[text]"], "He said \"hi\"");
    }

    /// Issue #1566: a quoted cell may hold a newline; the record goes on past it.
    #[test]
    fn a_quoted_cell_may_hold_a_newline() {
        let data = csv_with("qnl", "id,text\n1,\"line1\nline2\"\n2,next\n", ",");
        assert_eq!(data.lookup("1", "id")["[text]"], "line1\nline2");
        assert_eq!(data.lookup("2", "id")["[text]"], "next");
    }

    /// Issue #1566: unquoted cells are not trimmed, so a padded key matches only padded.
    #[test]
    fn unquoted_cells_keep_their_spaces() {
        let data = csv_with("spaces", "id,name\n padded , Spacey \n", ",");
        assert_eq!(data.lookup(" padded ", "id")["[name]"], " Spacey ");
        assert!(data.lookup("padded", "id").is_empty());
    }

    /// Issue #1566: headers are not trimmed either, so `[location]` is not ` location`'s token.
    #[test]
    fn headers_keep_their_spaces() {
        let data = csv_with(
            "hdr",
            "name, code,occupation, location\nbob,1,tgt,here\n",
            ",",
        );
        let row = data.lookup("tgt", "occupation");
        assert_eq!(row["[ location]"], "here");
        assert!(!row.contains_key("[location]"));
        assert!(data.lookup("1", "code").is_empty(), "` code` is the header");
    }

    /// Issue #1566: a row of another width than the header ends the file there, so a row after it
    /// does not exist, as in Mountebank's streamed parse.
    #[test]
    fn a_ragged_row_before_the_match_hides_it() {
        let data = csv_with("ragged_before", "id,name\n1,a,extra\n2,b\n", ",");
        assert!(data.lookup("2", "id").is_empty());
        assert!(data.lookup("1", "id").is_empty());
    }

    /// Issue #1566: a ragged row after the match leaves the rows before it served.
    #[test]
    fn a_ragged_row_after_the_match_still_serves_it() {
        let data = csv_with("ragged_after", "id,name\n1,a\n2,b,extra\n3,c\n", ",");
        assert_eq!(data.lookup("1", "id")["[name]"], "a");
        assert!(data.lookup("2", "id").is_empty());
        assert!(data.lookup("3", "id").is_empty());
    }

    /// Issue #1566: a `"` inside an unquoted cell is an error (`INVALID_OPENING_QUOTE`).
    #[test]
    fn a_quote_inside_an_unquoted_cell_ends_the_file() {
        let data = csv_with("midquote", "id,name\nba\"d,x\n2,b\n", ",");
        assert!(data.lookup("2", "id").is_empty());
        assert!(data.lookup("ba\"d", "id").is_empty());
    }

    /// Issue #1566: after a closing quote only a delimiter, a record end or EOF may follow
    /// (`CSV_INVALID_CLOSING_QUOTE`).
    #[test]
    fn text_after_a_closing_quote_ends_the_file() {
        let data = csv_with("after_close", "id,name\n\"tgt\" ,y\n2,b\n", ",");
        assert!(data.lookup("tgt", "id").is_empty());
        assert!(data.lookup("2", "id").is_empty());
    }

    /// Issue #1566: a quote left open to EOF is an error, but the rows before it are served.
    #[test]
    fn an_unclosed_quote_keeps_the_rows_before_it() {
        let data = csv_with("unclosed", "id,name\n1,a\n\"x,2\n", ",");
        assert_eq!(data.lookup("1", "id")["[name]"], "a");
        assert!(data.lookup("x,2\n", "id").is_empty());
    }

    /// Issue #1566: CRLF and lone-CR record ends both work; the cell carries no `\r`.
    #[test]
    fn crlf_and_cr_record_ends_are_read() {
        let crlf = csv_with("crlf", "id,name\r\n1,a\r\n2,b\r\n", ",");
        assert_eq!(crlf.lookup("2", "id")["[name]"], "b");
        let cr = csv_with("cr", "id,name\r1,a\r2,b\r", ",");
        assert_eq!(cr.lookup("2", "id")["[name]"], "b");
    }

    /// Issue #1566: the first record end seen fixes the sequence, so a later `\n` in a CRLF file
    /// is cell text, and the widened record ends the file (csv-parse's length error).
    #[test]
    fn the_first_record_end_fixes_the_sequence() {
        let data = csv_with("mixed", "id,name\r\n1,a\n2,b\n", ",");
        assert!(data.lookup("1", "id").is_empty());
        assert!(data.lookup("2", "id").is_empty());
    }

    /// Issue #1566: a blank line is a one-cell record, a width error after a wider header.
    #[test]
    fn a_blank_line_ends_the_file() {
        let data = csv_with("blank", "id,name\n1,a\n\n2,b\n", ",");
        assert_eq!(data.lookup("1", "id")["[name]"], "a");
        assert!(data.lookup("2", "id").is_empty());
    }

    /// Issue #1566: a UTF-8 BOM stays part of the first header, as csv-parse's default leaves it.
    #[test]
    fn a_bom_stays_in_the_first_header() {
        let data = csv_with("bom", "\u{feff}id,name\n1,a\n", ",");
        assert!(data.lookup("1", "id").is_empty());
        assert_eq!(data.lookup("1", "\u{feff}id")["[name]"], "a");
    }

    /// Issue #1566: a header row that fails to parse leaves no headers, so nothing matches.
    #[test]
    fn a_broken_header_row_matches_nothing() {
        let data = csv_with("bad_header", "i\"d,name\n1,a\n", ",");
        assert!(data.lookup("1", "name").is_empty());
        assert!(data.lookup("a", "name").is_empty());
    }

    /// Issue #1566: `delimiter` may be several characters, as csv-parse allows, and decodes.
    #[test]
    fn a_multi_character_delimiter_is_accepted() {
        let source: CsvDataSource = serde_json::from_value(serde_json::json!({
            "path": "x.csv", "keyColumn": "id", "delimiter": "||"
        }))
        .expect("a multi-character delimiter decodes");
        assert_eq!(
            serde_json::to_value(&source).expect("serializes")["delimiter"],
            "||"
        );
        let data = csv_with("multi", "id||name\n1||a|b\n2||\"x||y\"\n", "||");
        assert_eq!(data.lookup("1", "id")["[name]"], "a|b");
        assert_eq!(data.lookup("2", "id")["[name]"], "x||y");
    }

    /// Issue #1566: an empty `delimiter` means `,`, as Mountebank's `delimiter || ','` does.
    #[test]
    fn an_empty_delimiter_means_comma() {
        let source: CsvDataSource = serde_json::from_value(serde_json::json!({
            "path": "x.csv", "keyColumn": "id", "delimiter": ""
        }))
        .expect("an empty delimiter decodes");
        assert_eq!(source.path, "x.csv");
        let data = csv_with("empty_delim", "id,name\n1,a\n", "");
        assert_eq!(data.lookup("1", "id")["[name]"], "a");
    }

    /// Issue #1566: on 1000 seeded inputs (quotes, doubled quotes, `,` and `||`, every line
    /// break, spaces, non-ASCII), the parser keeps exactly the rows csv-parse 7.0.2 emits before
    /// its first error, and stops with the same error. `testdata/gen_csv_parse_oracle.js`
    /// regenerates the fixture.
    #[test]
    fn the_parser_agrees_with_csv_parse_7_0_2() {
        let fixture = include_str!("testdata/csv_parse_7_0_2.jsonl");
        let mut cases = 0;
        for line in fixture.lines() {
            let case: serde_json::Value = serde_json::from_str(line).expect("fixture line");
            let text = case["text"].as_str().expect("text");
            let delimiter = case["delimiter"].as_str().expect("delimiter");
            let expected_rows: Vec<Vec<String>> =
                serde_json::from_value(case["rows"].clone()).expect("rows");
            let expected_error = case["error"].as_str().map(|code| match code {
                "CSV_QUOTE_NOT_CLOSED" => CsvErrorKind::QuoteNotClosed,
                "CSV_INVALID_CLOSING_QUOTE" => CsvErrorKind::InvalidClosingQuote,
                "INVALID_OPENING_QUOTE" => CsvErrorKind::InvalidOpeningQuote,
                "CSV_RECORD_INCONSISTENT_FIELDS_LENGTH" => CsvErrorKind::InconsistentFieldsLength,
                other => panic!("unmapped csv-parse code {other}"),
            });
            let parsed = parse_csv(text, delimiter);
            assert_eq!(
                parsed.records, expected_rows,
                "rows for {text:?} / {delimiter:?}"
            );
            assert_eq!(
                parsed.error.map(|e| e.kind),
                expected_error,
                "error for {text:?} / {delimiter:?}"
            );
            cases += 1;
        }
        assert_eq!(cases, 1000);
    }

    /// Issue #1566: the reported line is the fault's, counting every line-break spelling.
    #[test]
    fn a_parse_error_names_its_line() {
        let ragged = parse_csv("id,name\r\n1,a\r\n2\r\n", ",")
            .error
            .expect("ragged");
        assert_eq!(ragged.kind, CsvErrorKind::InconsistentFieldsLength);
        assert_eq!(ragged.line, 3);
        let open = parse_csv("id,name\n1,\"a\nb\nc", ",")
            .error
            .expect("unclosed");
        assert_eq!(open.kind, CsvErrorKind::QuoteNotClosed);
        assert_eq!(open.line, 2, "an unclosed quote is reported where it opens");
    }

    /// Issue #1566: a trailing comma is an empty last cell; a missing final newline is fine.
    #[test]
    fn a_trailing_delimiter_is_an_empty_cell() {
        let data = csv_with("trailing", "id,name\n1,\n2,b", ",");
        assert_eq!(data.lookup("1", "id")["[name]"], "");
        assert_eq!(data.lookup("2", "id")["[name]"], "b");
    }

    /// Serves `template` in both the body and an `X-Row` header for a lookup keyed by the path
    /// `/ab-cd` (first group `ab` → row `first`).
    fn lookup_quoted(tag: &str, template: &str) -> (String, Vec<String>) {
        let csv =
            std::env::temp_dir().join(format!("rift_lookup_1402_{tag}_{}.csv", std::process::id()));
        std::fs::write(&csv, "id,name\nab,first\n").expect("write csv");
        let behavior: LookupBehavior = serde_json::from_value(serde_json::json!({
            "key": regex_key(None),
            "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
            "into": "${row}"
        }))
        .expect("the lookup parses");
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/ab-cd".to_string(),
            query: Default::default(),
            headers: crate::util::OrderedMap::default(),
            body: None,
            ..Default::default()
        };
        let mut headers: crate::imposter::Headers = crate::imposter::Headers::new();
        headers.insert("X-Row".to_string(), vec![template.to_string()]);
        let served = apply_lookup_behaviors(
            template,
            &mut headers,
            &[behavior],
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            FIXTURE_STUB,
        );
        let _ = std::fs::remove_file(&csv);
        (served, headers.shift_remove("X-Row").expect("header kept"))
    }

    /// Issue #1402: Mountebank's `replaceObjectValuesIn` replaces `${row}["col"]`,
    /// `${row}['col']` and `${row}[col]`; Rift used to replace only the unquoted form.
    #[test]
    fn a_double_quoted_column_token_is_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("dq", r#"<${row}["name"]>"#);
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn a_single_quoted_column_token_is_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("sq", "<${row}['name']>");
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn an_unquoted_column_token_is_still_replaced_in_body_and_header() {
        let (body, header) = lookup_quoted("uq", "<${row}[name]>");
        assert_eq!(body, "<first>");
        assert_eq!(header, vec!["<first>".to_string()]);
    }

    #[test]
    fn all_three_spellings_in_one_template_are_replaced() {
        let (body, header) = lookup_quoted("all", r#"${row}["id"]|${row}['name']|${row}[name]"#);
        assert_eq!(body, "ab|first|first");
        assert_eq!(header, vec!["ab|first|first".to_string()]);
    }

    /// Issue #1583: a missing key field is `undefined` in Mountebank's `lookupRow`, and a regex
    /// reads it as that text, so the row keyed `undefined` is served.
    #[test]
    fn a_missing_key_field_looks_up_the_row_keyed_undefined() {
        let key = serde_json::json!({
            "from": {"query": "nosuch"},
            "using": {"method": "regex", "selector": ".*"}
        });
        assert_eq!(lookup_with_key("undef", key, "/x", None), "from-undefined");
        let key = serde_json::json!({
            "from": {"query": "nosuch"},
            "using": {"method": "jsonpath", "selector": "$.a"}
        });
        assert_eq!(
            lookup_with_key("undef-jp", key, "/x", None),
            "${row}[name]",
            "a JSONPath selects nothing from undefined"
        );
    }

    /// Issue #1203 for header names (#1583): a client that sends a whole lookup token in its path
    /// gets it copied into a header name as text, never expanded by the lookup that follows.
    #[test]
    fn a_lookup_does_not_expand_a_token_a_copy_put_into_a_header_name() {
        let csv = std::env::temp_dir().join(format!("rift_lookup_1583_{}.csv", std::process::id()));
        std::fs::write(&csv, "id,name\nk,secret\n").expect("write csv");
        let copy: crate::behaviors::CopyBehavior = serde_json::from_value(serde_json::json!({
            "from": "path", "into": "${v}", "using": {"method": "regex", "selector": "[^/]+"}
        }))
        .expect("the copy parses");
        let lookup: LookupBehavior = serde_json::from_value(serde_json::json!({
            "key": {"from": {"query": "id"}, "using": {"method": "regex", "selector": ".*"}},
            "fromDataSource": {"csv": {"path": csv.to_string_lossy(), "keyColumn": "id"}},
            "into": "${row}"
        }))
        .expect("the lookup parses");
        let request = RequestContext {
            method: "GET".to_string(),
            path: "/${row}[name]".to_string(),
            query: crate::util::OrderedMap::from_iter([("id".to_string(), vec!["k".to_string()])]),
            ..Default::default()
        };
        let mut body = Spliced::authored("${v}|${row}[name]".to_string());
        let mut headers = authored_headers(crate::imposter::Headers::from([(
            "X-${v}".to_string(),
            vec!["1".to_string()],
        )]));
        crate::behaviors::apply_copy_spliced(
            &mut body,
            &mut headers,
            &mut None,
            &[copy],
            &request,
            FIXTURE_STUB,
        )
        .expect("copies");
        apply_lookup_spliced(
            &mut body,
            &mut headers,
            &mut None,
            &[lookup],
            &request,
            &CsvCache::default(),
            &ScriptFileAccess::trusted_cwd(),
            &BodyGuard::default(),
            FIXTURE_STUB,
        )
        .expect("the default budget holds the file");
        let _ = std::fs::remove_file(&csv);
        assert_eq!(
            body.as_str(),
            "${row}[name]|secret",
            "the authored token is expanded, the copied one is not"
        );
        assert_eq!(
            plain_headers(headers),
            crate::imposter::Headers::from([("X-${row}[name]".to_string(), vec!["1".to_string()])])
        );
    }

    // ===== Issue #1715: concurrent misses on one file share one read and parse =====

    /// Holds loads at their start until opened ([`CsvCache::close_gate`]).
    #[derive(Default)]
    pub(super) struct Gate {
        open: std::sync::Mutex<bool>,
        opened: std::sync::Condvar,
    }

    impl Gate {
        pub(super) fn wait(&self) {
            let mut open = self.open.lock().expect("gate lock");
            while !*open {
                open = self.opened.wait(open).expect("gate lock");
            }
        }

        pub(super) fn open(&self) {
            *self.open.lock().expect("gate lock") = true;
            self.opened.notify_all();
        }
    }

    /// A cache the serve path can hand to `spawn_blocking`, as the process-wide one is.
    fn leaked_cache() -> &'static CsvCache {
        Box::leak(Box::new(CsvCache::new()))
    }

    /// Wait until `cache` has `holders` requests in its flights, failing after 5 s.
    async fn until_holders(cache: &CsvCache, holders: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while cache.flight_holders() != holders {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{} holders, waiting for {holders}",
                cache.flight_holders()
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// `n` concurrent fetches of `path` through `guard`, spawned, not yet awaited.
    fn fetches(
        cache: &'static CsvCache,
        path: &str,
        guard: &Arc<BodyGuard>,
        n: usize,
    ) -> Vec<tokio::task::JoinHandle<LoadOutcome>> {
        (0..n)
            .map(|_| {
                let (path, guard) = (path.to_string(), Arc::clone(guard));
                tokio::spawn(async move {
                    cache
                        .fetch(&path, ",", Arc::new(ScriptFileAccess::trusted_cwd()), guard)
                        .await
                })
            })
            .collect()
    }

    async fn joined(tasks: Vec<tokio::task::JoinHandle<LoadOutcome>>) -> Vec<LoadOutcome> {
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.expect("fetch task"));
        }
        outcomes
    }

    fn loaded(outcome: &LoadOutcome) -> &CsvHandle {
        match outcome {
            LoadOutcome::Loaded(Some(handle)) => handle,
            other => panic!("expected a loaded parse, got {other:?}"),
        }
    }

    // Before #1715 every miss read and parsed the file under its own share of the budget, so with
    // room for one read, seven of eight concurrent requests were answered `503`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_misses_on_one_file_load_it_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 20 * 1024);
        let cache = leaked_cache();
        let guard = Arc::new(guard(64 * 1024, Some(64 * 1024)));
        // Room for one read of the file, not two.
        let _held = guard.try_reserve(40 * 1024).expect("40 KiB fits");
        let gate = cache.close_gate();
        let tasks = fetches(cache, &path, &guard, 8);
        until_holders(cache, 8).await;
        gate.open();
        let outcomes = joined(tasks).await;
        let first = Arc::clone(loaded(&outcomes[0]).data());
        for outcome in &outcomes {
            assert!(Arc::ptr_eq(loaded(outcome).data(), &first), "one parse");
        }
        assert_eq!(first.lookup("1", "id")["[name]"], "x");
        assert_eq!(cache.load_count(), 1);
        assert_eq!(cache.flight_holders(), 0);
        assert_eq!(cache.flight_count(), 0);
    }

    // A parse too large to cache is shared too, under its one share of the budget.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn waiters_share_one_uncached_parse_and_its_one_share() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 6 * 1024);
        let cache = leaked_cache();
        // The file fits the 8 KiB ceiling; its parse does not, so it is not cached.
        let guard = Arc::new(guard(8 * 1024, Some(64 * 1024)));
        let gate = cache.close_gate();
        let tasks = fetches(cache, &path, &guard, 4);
        until_holders(cache, 4).await;
        gate.open();
        let outcomes = joined(tasks).await;
        let first = Arc::clone(loaded(&outcomes[0]).data());
        for outcome in &outcomes {
            assert!(Arc::ptr_eq(loaded(outcome).data(), &first), "one parse");
        }
        assert_eq!(cache.load_count(), 1);
        assert!(cache.is_empty(), "too large to cache");
        assert!(
            guard.try_reserve(64 * 1024).is_err(),
            "the shared parse still holds its share"
        );
        drop(outcomes);
        drop(first);
        assert!(
            guard.try_reserve(64 * 1024).is_ok(),
            "released with the last handle"
        );
    }

    // Every request in a failed flight gets its failure; the next request loads afresh.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_flight_is_not_remembered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 4096);
        let cache = leaked_cache();
        let guard = Arc::new(guard(64 * 1024, Some(64 * 1024)));
        let held = guard.try_reserve(64 * 1024).expect("the whole budget");
        let gate = cache.close_gate();
        let tasks = fetches(cache, &path, &guard, 3);
        until_holders(cache, 3).await;
        gate.open();
        for outcome in joined(tasks).await {
            assert!(
                matches!(
                    outcome,
                    LoadOutcome::Exhausted(LookupLoadError::BudgetExhausted { .. })
                ),
                "{outcome:?}"
            );
        }
        assert_eq!(cache.load_count(), 1);
        assert_eq!(cache.flight_count(), 0);
        drop(held);
        let outcome = joined(fetches(cache, &path, &guard, 1)).await;
        assert_eq!(loaded(&outcome[0]).lookup("1", "id")["[name]"], "x");
        assert_eq!(cache.load_count(), 2);
        assert_eq!(cache.flight_count(), 0);
    }

    // The first request's client going away does not strand the others: the load passes on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_cancelled_leader_hands_the_load_to_a_waiter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 4096);
        let cache = leaked_cache();
        let guard = Arc::new(guard(64 * 1024, None));
        let gate = cache.close_gate();
        let leader = fetches(cache, &path, &guard, 1);
        until_holders(cache, 1).await;
        let waiter = fetches(cache, &path, &guard, 1);
        until_holders(cache, 2).await;
        for task in &leader {
            task.abort();
        }
        until_holders(cache, 1).await;
        gate.open();
        let outcome = joined(waiter).await;
        assert_eq!(loaded(&outcome[0]).lookup("1", "id")["[name]"], "x");
        assert_eq!(cache.flight_count(), 0);
    }

    // An edited file is a different version: a request that sees it does not wait on, or take,
    // the flight loading the old one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_edited_file_starts_a_new_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "old", 4096);
        let cache = leaked_cache();
        let guard = Arc::new(guard(64 * 1024, None));
        let gate = cache.close_gate();
        let first = fetches(cache, &path, &guard, 1);
        until_holders(cache, 1).await;
        padded_csv(dir.path(), "data.csv", "new", 8192);
        let second = fetches(cache, &path, &guard, 1);
        until_holders(cache, 2).await;
        assert_eq!(cache.flight_count(), 2);
        gate.open();
        let second = joined(second).await;
        assert_eq!(loaded(&second[0]).lookup("1", "id")["[name]"], "new");
        drop(joined(first).await);
        assert_eq!(cache.flight_count(), 0);
    }

    // Many requests finishing a flight together all give their place back: the flight never
    // outlives them, so no outcome is remembered for the next request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_flight_never_outlives_its_requests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = leaked_cache();
        let guard = Arc::new(guard(64 * 1024, None));
        for round in 0..50 {
            let path = padded_csv(dir.path(), &format!("data{round}.csv"), "x", 1024);
            for outcome in joined(fetches(cache, &path, &guard, 16)).await {
                assert_eq!(loaded(&outcome).lookup("1", "id")["[name]"], "x");
            }
            assert_eq!(cache.flight_count(), 0, "round {round}");
        }
    }

    // Two managers' budgets are never charged for each other: each guard is its own flight.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_guards_do_not_share_a_flight() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = padded_csv(dir.path(), "data.csv", "x", 4096);
        let cache = leaked_cache();
        let (one, two) = (
            Arc::new(guard(64 * 1024, None)),
            Arc::new(guard(64 * 1024, None)),
        );
        let gate = cache.close_gate();
        let mut tasks = fetches(cache, &path, &one, 1);
        tasks.extend(fetches(cache, &path, &two, 1));
        until_holders(cache, 2).await;
        assert_eq!(cache.flight_count(), 2);
        gate.open();
        for outcome in joined(tasks).await {
            assert_eq!(loaded(&outcome).lookup("1", "id")["[name]"], "x");
        }
        assert_eq!(cache.load_count(), 2);
    }
}
