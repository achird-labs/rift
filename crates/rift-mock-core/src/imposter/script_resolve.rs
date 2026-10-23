//! Config-time resolution for `_rift.script` `file:`/`ref:` sources (issue #356).
//!
//! A `RiftScriptConfig` may specify its script via inline `code`, a `file` path, or a `ref` into
//! the imposter's `_rift.scripts` named registry. This module walks a freshly-parsed
//! `ImposterConfig`, validates that every script config carries exactly one source, and resolves
//! `file`/`ref` down to a populated `code` + `engine` — so everything downstream (validation,
//! execution, the compiled-script cache) only ever sees plain inline scripts.
//!
//! Resolution happens once, at config-load time (`--configfile`/`--datadir`, admin API create,
//! and `/admin/reload`), never per-request.

use super::types::{ImposterConfig, RiftScriptConfig, Stub, StubResponse};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Errors from resolving `file:`/`ref:` script sources.
#[derive(Debug, thiserror::Error)]
pub enum ScriptResolveError {
    #[error("script config must specify exactly one of `code`, `file`, or `ref` (found {found})")]
    InvalidSourceCount { found: usize },
    #[error("unknown script ref '{0}': no entry named '{0}' in `_rift.scripts`")]
    UnknownRef(String),
    #[error("_rift.scripts entry '{0}' may not itself use `ref` — ref chains are not allowed")]
    RefChain(String),
    #[error("failed to read script file '{path}': {source}")]
    FileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("script file path '{0}' escapes the scripts root")]
    PathEscape(String),
    #[error(
        "script file '{0}' cannot be resolved: no --scripts-dir is configured for the admin API"
    )]
    ScriptsDirNotConfigured(String),
}

/// Where `file:` script paths resolve relative to, and whether escaping that root is rejected.
#[derive(Debug, Clone)]
pub enum ScriptBaseDir {
    /// Explicit `--configfile` single-file load: relative to the config file's own directory. No
    /// escape restriction — a human author who can write the configfile already has filesystem
    /// access, so an absolute path or `..` is honored verbatim (matches EJS `include`).
    ConfigRelative(PathBuf),
    /// `--datadir` load (including the persisted `{port}.json` files written by the admin API):
    /// relative to the datadir root, with the SAME escape rules as [`ScriptBaseDir::ScriptsDir`].
    /// These files can be network-authored (a stub POSTed through the admin API is persisted
    /// here), so an absolute path or a `..`/symlink escape is rejected — never read (issue #356
    /// B1/B2 defense-in-depth against a datadir re-resolution reading `/etc/passwd`).
    DatadirRelative(PathBuf),
    /// Admin-API-created imposters: relative to `--scripts-dir`. A resolved path that would
    /// escape this root is rejected outright (never read).
    ScriptsDir(PathBuf),
    /// The admin API has no `--scripts-dir` configured: any `file:` reference is rejected.
    Unconfigured,
}

/// Resolve every `_rift.script` in `config` — the named registry first, then each stub response
/// — mutating `code`/`engine` in place. Returns the first error encountered; on error `config`
/// may be partially mutated and must not be used (the caller returns the error to the client /
/// aborts the load without applying anything).
pub fn resolve_scripts(
    config: &mut ImposterConfig,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    let default_engine = config.default_script_engine().to_owned();
    // Resolve the named registry first so response-level `ref:` lookups see fully-resolved
    // entries (populated `code` + `engine`, never another `ref`).
    if let Some(rift) = &mut config.rift {
        for (name, script) in rift.scripts.iter_mut() {
            if script.ref_name.is_some() {
                return Err(ScriptResolveError::RefChain(name.clone()));
            }
            validate_source_count(script)?;
            resolve_leaf(script, &default_engine, base)?;
        }
    }

    // Snapshot the resolved registry (small, config-time only) so per-response resolution can
    // look up `ref:` targets without holding a borrow of `config.rift` across the stub loop.
    let registry: HashMap<String, RiftScriptConfig> = config
        .rift
        .as_ref()
        .map(|r| r.scripts.clone())
        .unwrap_or_default();

    resolve_stub_scripts(&mut config.stubs, &registry, &default_engine, base)
}

/// Resolve `_rift.script` sources in a set of stubs against an already-resolved `registry`
/// (issue #356). Used by the admin-API stub sub-resource endpoints (`POST/PUT .../stubs[...]`,
/// space stubs), which add stubs to an existing imposter and must resolve — and therefore
/// escape-check — `file:`/`ref:` at WRITE time, before persisting, exactly like whole-imposter
/// create. `registry` is the target imposter's `_rift.scripts` (its entries must themselves be
/// already resolved — `code` populated, `file`/`ref` cleared — which they are once the imposter
/// was created). `default_engine` is the target imposter's
/// [`ImposterConfig::default_script_engine`]: a stub arrives without its imposter's `_rift` block,
/// so a door that passed `"rhai"` here would ignore the imposter's `defaultEngine` (issue #1159).
pub fn resolve_stub_scripts(
    stubs: &mut [Stub],
    registry: &HashMap<String, RiftScriptConfig>,
    default_engine: &str,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    for stub in stubs {
        for response in &mut stub.responses {
            if let Some(script) = response_script_mut(response) {
                resolve_response_script(script, registry, default_engine, base)?;
            }
        }
    }
    Ok(())
}

/// Borrow the `_rift.script` config out of a stub response, if it has one — covers both the
/// `is` response's optional `_rift` extension and the script-only `RiftScript` response.
fn response_script_mut(response: &mut StubResponse) -> Option<&mut RiftScriptConfig> {
    match response {
        StubResponse::Is {
            rift: Some(rift), ..
        } => rift.script.as_mut(),
        StubResponse::RiftScript { rift, .. } => rift.script.as_mut(),
        _ => None,
    }
}

fn validate_source_count(script: &RiftScriptConfig) -> Result<(), ScriptResolveError> {
    if script.has_valid_source() {
        Ok(())
    } else {
        Err(ScriptResolveError::InvalidSourceCount {
            found: script.source_count(),
        })
    }
}

/// Resolve a single response-level script: validate its source count, then either follow `ref:`
/// into the (already-resolved) registry or resolve it as a leaf (`code:`/`file:`).
fn resolve_response_script(
    script: &mut RiftScriptConfig,
    registry: &HashMap<String, RiftScriptConfig>,
    default_engine: &str,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    validate_source_count(script)?;
    if let Some(ref_name) = script.ref_name.clone() {
        let target = registry
            .get(&ref_name)
            .ok_or_else(|| ScriptResolveError::UnknownRef(ref_name.clone()))?;
        script.code = target.code.clone();
        script.engine = target.engine.clone();
        // `ref` fully collapses into `code`/`engine` — clearing it keeps `source_count() == 1`
        // for any post-resolution re-check (e.g. `validate_stubs`, run after resolution in the
        // admin-API handlers) and matches the resolved leaf's shape below.
        script.ref_name = None;
        return Ok(());
    }
    resolve_leaf(script, default_engine, base)
}

/// Resolve a leaf script config (`code:` or `file:`, never `ref:`): load `file:` content into
/// `code` if needed, then normalize `engine` to the effective value: explicit, else inferred from
/// the file extension, else the imposter's `defaultEngine` (issue #1159). Writing it back keeps a
/// persisted or replayed imposter unambiguous.
///
/// `file` is cleared once its content is loaded into `code` — resolution is meant to collapse
/// every source down to a plain `code`/`engine` pair, so everything downstream (validation,
/// execution, the compiled-script cache) only ever sees an inline script, and a repeated
/// `source_count()` check (post-resolution) still sees exactly one source.
fn resolve_leaf(
    script: &mut RiftScriptConfig,
    default_engine: &str,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    if script.engine.is_none() {
        script.engine = Some(infer_engine(script.file.as_deref(), default_engine));
    }
    if let Some(file) = script.file.take() {
        script.code = Some(read_script_file(&file, base)?);
    }
    Ok(())
}

/// Infer the engine from a `file:` path's extension; `.rhai`/`.lua`/`.js` map to their engines, and
/// anything else (or no `file`, i.e. inline `code:`) takes `default_engine`. The extension outranks
/// the default on purpose: a `.rhai` file under `defaultEngine: "javascript"` is still Rhai.
fn infer_engine(file: Option<&str>, default_engine: &str) -> String {
    let ext = file
        .and_then(|f| Path::new(f).extension())
        .and_then(|e| e.to_str());
    match ext {
        Some("rhai") => "rhai",
        Some("lua") => "lua",
        Some("js") => "javascript",
        _ => default_engine,
    }
    .to_string()
}

/// Read a `file:` script's content per `base`. For the escape-checked roots (`ScriptsDir` /
/// `DatadirRelative`) the path is resolved AND canonicalized-under-root BEFORE any read (issue
/// #356 B5), so a symlink inside the root that points outside is rejected without its target ever
/// being read, and a canonicalize failure fails closed (reject) rather than open.
fn read_script_file(file: &str, base: &ScriptBaseDir) -> Result<String, ScriptResolveError> {
    match base {
        ScriptBaseDir::Unconfigured => Err(ScriptResolveError::ScriptsDirNotConfigured(
            file.to_string(),
        )),
        // Explicit --configfile: human-authored, any path allowed (no escape check).
        ScriptBaseDir::ConfigRelative(dir) => {
            let path = dir.join(file);
            std::fs::read_to_string(&path).map_err(|source| ScriptResolveError::FileRead {
                path: file.to_string(),
                source,
            })
        }
        // Escape-checked roots: resolve strictly under `root`, then read the vetted path.
        ScriptBaseDir::ScriptsDir(root) | ScriptBaseDir::DatadirRelative(root) => {
            let vetted = resolve_within_root(root, file)?;
            std::fs::read_to_string(&vetted).map_err(|source| ScriptResolveError::FileRead {
                path: file.to_string(),
                source,
            })
        }
    }
}

/// Resolve `file` strictly under `root`, returning the vetted on-disk path to read, or an error —
/// WITHOUT reading the file (issue #356 B5, fail-closed). In order:
///  1. lexically reject an absolute path or a `..` that climbs above `root` ([`safe_join`]);
///  2. canonicalize the candidate and require it to stay under the canonical `root` — this
///     follows symlinks, so a link inside the root pointing outside resolves outside and is
///     rejected before its target is read;
///  3. if canonicalization fails, distinguish a genuinely-missing file (its parent still resolves
///     under the root → surface a `FileRead` so the message names the missing file) from anything
///     else (a broken/escaping symlink, an unreadable parent) → reject as `PathEscape`. Either way
///     the target is never read here.
fn resolve_within_root(root: &Path, file: &str) -> Result<PathBuf, ScriptResolveError> {
    let candidate = safe_join(root, file)?;
    let canon_root = root
        .canonicalize()
        .map_err(|source| ScriptResolveError::FileRead {
            path: file.to_string(),
            source,
        })?;
    match candidate.canonicalize() {
        Ok(canon_candidate) => {
            if canon_candidate.starts_with(&canon_root) {
                Ok(canon_candidate)
            } else {
                Err(ScriptResolveError::PathEscape(file.to_string()))
            }
        }
        Err(source) => {
            // The candidate itself doesn't resolve. If its PARENT canonicalizes under the root
            // it's simply a missing file (safe to report as such); otherwise fail closed.
            let parent_ok = candidate
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .is_some_and(|cp| cp.starts_with(&canon_root));
            if parent_ok {
                Err(ScriptResolveError::FileRead {
                    path: file.to_string(),
                    source,
                })
            } else {
                Err(ScriptResolveError::PathEscape(file.to_string()))
            }
        }
    }
}

/// Lexically join `root` with `rel`, rejecting any absolute path or `..` component that would
/// climb above `root` — computed purely from path components (no filesystem access), so an
/// escaping reference is rejected before the file is ever opened.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, ScriptResolveError> {
    let mut joined = root.to_path_buf();
    let mut depth: i32 = 0;
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(part) => {
                joined.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(ScriptResolveError::PathEscape(rel.to_string()));
                }
                joined.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ScriptResolveError::PathEscape(rel.to_string()));
            }
        }
    }
    Ok(joined)
}

/// What a script running in an imposter may read from disk at request time (issue #1611): the
/// modules a Mountebank-style hook loads with `require()`.
///
/// This is the runtime counterpart of [`ScriptBaseDir`], which vets a `file:` path once, at
/// admission. A `require` path is computed by code on every request, so it is vetted here, each
/// time it is used. The mode belongs to the running imposter and is chosen by the door that wrote
/// it; it is never part of [`ImposterConfig`], so it is never serialized, never a `stub_key` input
/// and never in the bytes an embedder replicates.
///
/// None of this is a sandbox: `--allowInjection` also admits `shellTransform`, which runs host
/// commands. Confinement keeps a network-authored script from reading the host's files by the
/// route that looks harmless.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScriptFileAccess {
    /// A network-authored door with `--scripts-dir`: only modules under this root load.
    Confined(PathBuf),
    /// A network-authored door without `--scripts-dir`, and any embedder that has not chosen:
    /// every `require` is refused.
    #[default]
    Refused,
    /// A local, human-authored door (`--configfile`, the in-process FFI APIs): any path, a relative
    /// one joined to `base`. Its author already has the filesystem.
    Trusted { base: PathBuf },
}

impl ScriptFileAccess {
    /// The access a network-authored door grants: confined to `scripts_dir`, or refused without
    /// one.
    #[must_use]
    pub fn network(scripts_dir: Option<&Path>) -> Self {
        scripts_dir.map_or(Self::Refused, |dir| Self::Confined(dir.to_path_buf()))
    }

    /// The access a local, human-authored door grants: unconfined, with a relative path joined to
    /// the process's working directory, as `require` has always resolved one.
    #[must_use]
    pub fn trusted_cwd() -> Self {
        // Domain-equivalent fallback, not a swallow: `.` is the working directory, so a relative
        // path still resolves against it when the absolute form cannot be read.
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::Trusted { base }
    }

    /// The access that allows no more than either (issue #1645): what a running imposter's scripts
    /// keep when a door writes some of its stubs and leaves the rest. Two different roots allow
    /// nothing in common, so they meet at [`Self::Refused`]; two trusted doors keep `other`'s base.
    #[must_use]
    pub fn meet(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Trusted { .. }, other) => other.clone(),
            (narrower, Self::Trusted { .. }) => narrower.clone(),
            (Self::Confined(a), Self::Confined(b)) if a == b => self.clone(),
            _ => Self::Refused,
        }
    }

    /// Whether `other` allows no more than `self`, so an imposter's access can become `other` in
    /// place without any of its stubs gaining access (issue #1645).
    #[must_use]
    pub fn admits(&self, other: &Self) -> bool {
        self.meet(other) == *other
    }
}

/// Why [`resolve_confined`] refused a path.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfinedPathError {
    /// The path is outside the root, or reaches outside through a link. One answer for every such
    /// path, whether or not anything exists there.
    #[error("outside the root")]
    Outside,
    /// The path is inside the root and nothing is there.
    #[error("no such file under the root")]
    NotFound,
    /// The root itself cannot be resolved.
    #[error("the root cannot be resolved: {0}")]
    Root(std::io::Error),
    /// The vetted file could not be opened or read (see [`read_confined`]).
    #[error("{0}")]
    Read(std::io::Error),
}

/// Resolve `request` to a file strictly under `root`, without reading it (issue #1611).
///
/// A relative `request` joins `base` (the root itself, or the directory of the module asking); an
/// absolute one is taken as written. Either way the path is first normalised lexically, and one
/// outside the root is refused before the filesystem is touched. A path inside it is then
/// canonicalised and must still be under the canonical root, so a link inside the root that
/// points out is refused as well. Every refusal of a path outside the root is the same
/// [`ConfinedPathError::Outside`], so the answer never depends on what exists outside it.
///
/// The returned path is canonical; read that one.
pub(crate) fn resolve_confined(
    root: &Path,
    base: &Path,
    request: &str,
) -> Result<PathBuf, ConfinedPathError> {
    let lexical_root =
        normalize_lexically(&std::path::absolute(root).map_err(ConfinedPathError::Root)?);
    let canonical_root = lexical_root
        .canonicalize()
        .map_err(ConfinedPathError::Root)?;
    let requested = Path::new(request);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        std::path::absolute(base)
            .map_err(ConfinedPathError::Root)?
            .join(requested)
    };
    let candidate = normalize_lexically(&joined);
    // Either spelling of the root: a module's own directory is canonical, a path written into a
    // script usually is not.
    if !candidate.starts_with(&lexical_root) && !candidate.starts_with(&canonical_root) {
        return Err(ConfinedPathError::Outside);
    }
    match candidate.canonicalize() {
        Ok(canonical) if canonical.starts_with(&canonical_root) => Ok(canonical),
        Ok(_) => Err(ConfinedPathError::Outside),
        Err(e) => {
            // A link whose target is missing is refused like any link out, never reported as a
            // missing file: that would say whether its target exists.
            let is_link = candidate
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink());
            let parent_inside = candidate
                .parent()
                .and_then(|parent| parent.canonicalize().ok())
                .is_some_and(|parent| parent.starts_with(&canonical_root));
            if !is_link && parent_inside && e.kind() == std::io::ErrorKind::NotFound {
                Err(ConfinedPathError::NotFound)
            } else {
                Err(ConfinedPathError::Outside)
            }
        }
    }
}

/// Read the file [`resolve_confined`] returned as `canonical` for `request`, failing closed if what
/// is opened is not that file (issues #1611, #1617).
///
/// A link swapped into the path between resolving it and opening it would let the open reach a
/// file outside the root. So the file is opened first, `request` is resolved again, and the read
/// goes ahead only when the second resolution is the same path and names the very file that is
/// open (same device and inode on Unix). Off Unix that identity check is path-only: the second
/// resolution must land on the same canonical path. Anything else is
/// [`ConfinedPathError::Outside`], the same answer as any other path out of the root.
///
/// Only a regular file is read. The open does not block (`O_NONBLOCK` on Unix), so a FIFO or a
/// device, under the root or swapped in during the race, cannot hold a worker at `open`, and
/// anything that is not a regular file once open, a directory included, is refused as
/// [`ConfinedPathError::Outside`] before it is resolved again or read. Links inside the root are
/// still followed.
// Only the JavaScript engine's `require()` reads a confined file whole; a `lookup` CSV opens it with
// [`open_confined`] and reads it under the body limits (issue #1654).
#[cfg_attr(not(feature = "javascript"), allow(dead_code))]
pub(crate) fn read_confined(
    root: &Path,
    base: &Path,
    request: &str,
    canonical: &Path,
) -> Result<String, ConfinedPathError> {
    use std::io::Read;
    let mut file = open_confined(root, base, request, canonical)?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(ConfinedPathError::Read)?;
    Ok(text)
}

/// [`read_confined`]'s checks, returning the vetted file open and unread, for a caller that reads
/// it under limits of its own (a `lookup` CSV, issue #1654).
pub(crate) fn open_confined(
    root: &Path,
    base: &Path,
    request: &str,
    canonical: &Path,
) -> Result<std::fs::File, ConfinedPathError> {
    let file = open_without_blocking(canonical).map_err(ConfinedPathError::Read)?;
    let opened = file.metadata().map_err(ConfinedPathError::Read)?;
    if !opened.is_file() {
        return Err(ConfinedPathError::Outside);
    }
    let again = resolve_confined(root, base, request)?;
    // Fails closed: a path that no longer resolves to a readable file was changed under the read.
    let current = std::fs::metadata(&again).map_err(|_| ConfinedPathError::Outside)?;
    if again != canonical || !same_file(&opened, &current) {
        return Err(ConfinedPathError::Outside);
    }
    Ok(file)
}

/// Open `path` for reading without waiting on it: a FIFO with no writer, or a device, would
/// otherwise block `open` itself. `O_NONBLOCK` changes nothing for a regular file's reads.
#[cfg(unix)]
fn open_without_blocking(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_without_blocking(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Whether two metadata reads describe the same file.
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Off Unix there is no stable file identity in `std`; the second resolution above still has to
/// land on the same canonical path.
#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// Why a `lookup` behavior may not read the CSV file its `fromDataSource.csv.path` names (issue
/// #1617). The path is the one the config wrote; nothing in a message depends on what exists
/// outside `--scripts-dir`.
#[derive(Debug, thiserror::Error)]
pub enum LookupFileError {
    /// The path is absolute outside the root, climbs out of it with `..`, or reaches out through a
    /// link. One answer for all of them, whether or not anything exists there.
    #[error("lookup CSV path '{0}' is outside --scripts-dir")]
    Outside(String),
    /// The imposter was written by a network door and the server has no `--scripts-dir`.
    #[error("lookup CSV path '{0}' cannot be read: no --scripts-dir is configured")]
    NoRoot(String),
    /// `--scripts-dir` itself cannot be resolved.
    #[error("lookup CSV path '{path}' cannot be read: --scripts-dir cannot be resolved: {source}")]
    Root {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// The path is allowed and nothing readable is there (yet).
    #[error("lookup CSV path '{path}' cannot be read: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl LookupFileError {
    /// Whether a config door refuses a lookup that fails this way. A missing file is admitted, as
    /// in Mountebank: the file may be written after the imposter. Every other failure is a path
    /// the imposter could never read.
    #[must_use]
    pub fn refuses_admission(&self) -> bool {
        !matches!(self, Self::Unreadable { .. })
    }
}

/// The file a `lookup` behavior's CSV `path` names, under the imposter's [`ScriptFileAccess`]
/// (issue #1617). Mountebank reads any path; Rift reads, for an imposter written by a network door
/// (the admin API, `--datadir`, an `https:` source), only files under `--scripts-dir`, a relative
/// path from there, and none without it. A local door's imposter ([`ScriptFileAccess::Trusted`])
/// reads any path, a relative one from its base, as before.
///
/// The result is canonical, so it is also the cache key. Read it with [`read_lookup_file`].
///
/// # Errors
/// See [`LookupFileError`].
pub fn resolve_lookup_path(
    path: &str,
    access: &ScriptFileAccess,
) -> Result<PathBuf, LookupFileError> {
    match access {
        ScriptFileAccess::Refused => Err(LookupFileError::NoRoot(path.to_string())),
        ScriptFileAccess::Confined(root) => {
            resolve_confined(root, root, path).map_err(|e| lookup_error(path, e))
        }
        ScriptFileAccess::Trusted { base } => {
            base.join(path)
                .canonicalize()
                .map_err(|source| LookupFileError::Unreadable {
                    path: path.to_string(),
                    source,
                })
        }
    }
}

/// Read the file [`resolve_lookup_path`] resolved `path` to, re-checking it under a confined
/// access at the moment of the read ([`read_confined`]), since files and links can change after
/// the imposter was admitted.
///
/// # Errors
/// See [`LookupFileError`].
pub fn read_lookup_file(
    path: &str,
    resolved: &Path,
    access: &ScriptFileAccess,
) -> Result<String, LookupFileError> {
    use std::io::Read;
    let mut text = String::new();
    open_lookup_file(path, resolved, access)?
        .read_to_string(&mut text)
        .map_err(|source| LookupFileError::Unreadable {
            path: path.to_string(),
            source,
        })?;
    Ok(text)
}

/// [`read_lookup_file`]'s checks, returning the file open and unread, so the caller can read it
/// under the body limits (issue #1654).
///
/// # Errors
/// See [`LookupFileError`].
pub fn open_lookup_file(
    path: &str,
    resolved: &Path,
    access: &ScriptFileAccess,
) -> Result<std::fs::File, LookupFileError> {
    match access {
        ScriptFileAccess::Refused => Err(LookupFileError::NoRoot(path.to_string())),
        ScriptFileAccess::Confined(root) => {
            open_confined(root, root, path, resolved).map_err(|e| lookup_error(path, e))
        }
        // Opened without blocking and refused unless a regular file, as a confined read is: a
        // FIFO or a device would otherwise hold the reading thread (issue #1654).
        ScriptFileAccess::Trusted { .. } => {
            let unreadable = |source| LookupFileError::Unreadable {
                path: path.to_string(),
                source,
            };
            let file = open_without_blocking(resolved).map_err(unreadable)?;
            let regular = file.metadata().map_err(unreadable)?.is_file();
            if !regular {
                return Err(unreadable(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "not a regular file",
                )));
            }
            Ok(file)
        }
    }
}

fn lookup_error(path: &str, e: ConfinedPathError) -> LookupFileError {
    let path = path.to_string();
    match e {
        ConfinedPathError::Outside => LookupFileError::Outside(path),
        ConfinedPathError::NotFound => LookupFileError::Unreadable {
            path,
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file under --scripts-dir",
            ),
        },
        ConfinedPathError::Root(source) => LookupFileError::Root { path, source },
        ConfinedPathError::Read(source) => LookupFileError::Unreadable { path, source },
    }
}

/// Refuse a stub whose `lookup` behaviors name a CSV file its imposter could never read under
/// `access` (issue #1617): a path outside `--scripts-dir`, or any path when a network door has no
/// `--scripts-dir`. Run by the admin API's write doors with the access they give the imposter.
///
/// Deliberately not part of [`crate::imposter::admission_check`] or any `Deserialize`: what is
/// readable depends on the door and the server's flags, not on the config, so a stored or replayed
/// config is never refused for it. Such an imposter loads and its lookup is refused when it runs,
/// by [`resolve_lookup_path`].
///
/// # Errors
/// The first refused lookup's message.
pub fn check_lookup_paths(stubs: &[Stub], access: &ScriptFileAccess) -> Result<(), String> {
    for path in stubs.iter().flat_map(executed_lookup_paths) {
        if let Err(e) = resolve_lookup_path(&path, access)
            && e.refuses_admission()
        {
            return Err(e.to_string());
        }
    }
    Ok(())
}

/// The CSV paths of the `lookup` behaviors a stub runs. Only the response types that run their
/// behaviors count; a `fault` or `_rift.script` response's behaviors are ignored, so never read.
fn executed_lookup_paths(stub: &Stub) -> Vec<String> {
    stub.responses
        .iter()
        .filter(|response| {
            matches!(
                response,
                StubResponse::Is { .. } | StubResponse::Inject { .. } | StubResponse::Proxy { .. }
            )
        })
        .filter_map(StubResponse::behaviors_block)
        .filter_map(serde_json::Value::as_array)
        // Domain-optional: a program that does not parse is refused by the doors' behaviors check
        // and never runs, so it reads no file.
        .filter_map(|elements| crate::behaviors::BehaviorProgram::from_elements(elements).ok())
        .flat_map(|program| program.steps)
        .filter_map(|step| match step {
            crate::behaviors::BehaviorStep::Lookup(lookup) => {
                Some(lookup.from_data_source.csv.path)
            }
            _ => None,
        })
        .collect()
}

/// `path` with every `.` dropped and every `..` applied to what precedes it, computed from its
/// components alone. A `..` at the top stays at the top, as the filesystem treats it.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::types::{RiftConfig, RiftResponseExtension, Stub};

    fn script(code: Option<&str>, file: Option<&str>, ref_name: Option<&str>) -> RiftScriptConfig {
        RiftScriptConfig {
            engine: None,
            code: code.map(str::to_string),
            file: file.map(str::to_string),
            ref_name: ref_name.map(str::to_string),
        }
    }

    fn stub_with_script(script_config: RiftScriptConfig) -> Stub {
        Stub {
            name: None,
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::RiftScript {
                rift: Box::new(RiftResponseExtension {
                    dataset: None,
                    conditional: None,
                    fault: None,
                    script: Some(script_config),
                    templated: false,
                    state_ops: Vec::new(),
                }),
                ignored_behaviors: None,
            }],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        }
    }

    fn extract_script(config: &ImposterConfig) -> &RiftScriptConfig {
        match &config.stubs[0].responses[0] {
            StubResponse::RiftScript { rift, .. } => rift.script.as_ref().unwrap(),
            other => panic!("expected RiftScript, got {other:?}"),
        }
    }

    #[test]
    fn inline_code_resolves_with_default_engine() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(
                Some("fn respond() {}"),
                None,
                None,
            ))],
            ..Default::default()
        };
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("fn respond() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
    }

    fn with_default_engine(config: &mut ImposterConfig, engine: &str) {
        config.rift = Some(
            serde_json::from_value(serde_json::json!({
                "scriptEngine": { "defaultEngine": engine }
            }))
            .expect("rift config"),
        );
    }

    /// Issue #1159: the imposter's `defaultEngine` was parsed and never read; inline code always
    /// resolved to Rhai.
    #[test]
    fn inline_code_takes_the_imposters_default_engine() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(
                Some("function respond() {}"),
                None,
                None,
            ))],
            ..Default::default()
        };
        with_default_engine(&mut config, "javascript");
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
        assert_eq!(
            extract_script(&config).engine.as_deref(),
            Some("javascript")
        );
    }

    #[test]
    fn an_explicit_engine_outranks_the_default() {
        let mut explicit = script(Some("fn respond() {}"), None, None);
        explicit.engine = Some("rhai".to_owned());
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(explicit)],
            ..Default::default()
        };
        with_default_engine(&mut config, "javascript");
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
        assert_eq!(extract_script(&config).engine.as_deref(), Some("rhai"));
    }

    #[test]
    fn a_file_extension_outranks_the_default() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("s.rhai"), "fn respond() {}").expect("write");
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("s.rhai"), None))],
            ..Default::default()
        };
        with_default_engine(&mut config, "javascript");
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ScriptsDir(root.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(extract_script(&config).engine.as_deref(), Some("rhai"));
    }

    #[test]
    fn a_file_without_a_known_extension_takes_the_default() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("s.txt"), "function respond() {}").expect("write");
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("s.txt"), None))],
            ..Default::default()
        };
        with_default_engine(&mut config, "javascript");
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ScriptsDir(root.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            extract_script(&config).engine.as_deref(),
            Some("javascript")
        );
    }

    /// The stub doors pass the live imposter's default; this is the seam they call.
    #[test]
    fn stub_resolution_uses_the_default_it_is_given() {
        let mut stubs = vec![stub_with_script(script(Some("x"), None, None))];
        resolve_stub_scripts(
            &mut stubs,
            &HashMap::new(),
            "javascript",
            &ScriptBaseDir::Unconfigured,
        )
        .unwrap();
        let config = ImposterConfig {
            stubs,
            ..Default::default()
        };
        assert_eq!(
            extract_script(&config).engine.as_deref(),
            Some("javascript")
        );
    }

    /// A stale `"lua"` default with no engine-less script is never used, so it is not refused at
    /// resolution — such configs load today and must keep loading.
    #[test]
    fn an_unused_unknown_default_is_not_refused() {
        let mut explicit = script(Some("fn respond() {}"), None, None);
        explicit.engine = Some("rhai".to_owned());
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(explicit)],
            ..Default::default()
        };
        with_default_engine(&mut config, "lua");
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
    }

    #[test]
    fn zero_sources_is_rejected() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, None, None))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap_err();
        assert!(matches!(
            err,
            ScriptResolveError::InvalidSourceCount { found: 0 }
        ));
    }

    #[test]
    fn multiple_sources_is_rejected() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(Some("x"), Some("y.rhai"), None))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap_err();
        assert!(matches!(
            err,
            ScriptResolveError::InvalidSourceCount { found: 2 }
        ));
    }

    #[test]
    fn file_resolves_relative_to_config_dir_and_infers_engine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("script.lua"), "return {inject=false}").unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("script.lua"), None))],
            ..Default::default()
        };
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ConfigRelative(dir.path().to_path_buf()),
        )
        .unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("return {inject=false}"));
        assert_eq!(resolved.engine.as_deref(), Some("lua"));
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("nope.rhai"), None))],
            ..Default::default()
        };
        let err = resolve_scripts(
            &mut config,
            &ScriptBaseDir::ConfigRelative(dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(matches!(err, ScriptResolveError::FileRead { .. }));
    }

    #[test]
    fn scripts_dir_unconfigured_rejects_file_reference() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("a.rhai"), None))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap_err();
        assert!(matches!(
            err,
            ScriptResolveError::ScriptsDirNotConfigured(_)
        ));
    }

    #[test]
    fn scripts_dir_escape_is_rejected_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        // A real file that a naive join would successfully read if the escape check were
        // skipped — its presence proves the rejection is the `..` check, not a missing file.
        std::fs::write(dir.path().join("secret.rhai"), "SECRET").unwrap();
        let root = dir.path().join("scripts");
        std::fs::create_dir(&root).unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("../secret.rhai"), None))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::ScriptsDir(root)).unwrap_err();
        assert!(matches!(err, ScriptResolveError::PathEscape(_)));
    }

    #[test]
    fn scripts_dir_within_root_is_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ok.rhai"), "fn respond() {}").unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("ok.rhai"), None))],
            ..Default::default()
        };
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ScriptsDir(dir.path().to_path_buf()),
        )
        .unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("fn respond() {}"));
    }

    #[test]
    fn ref_resolves_from_registry() {
        let mut scripts = HashMap::new();
        scripts.insert(
            "failTwice".to_string(),
            script(Some("fn respond() {}"), None, None),
        );
        let mut config = ImposterConfig {
            rift: Some(RiftConfig {
                scripts,
                ..Default::default()
            }),
            stubs: vec![stub_with_script(script(None, None, Some("failTwice")))],
            ..Default::default()
        };
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("fn respond() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
    }

    #[test]
    fn ref_resolves_a_file_backed_registry_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fail-twice.rhai"), "fn respond() {}").unwrap();
        let mut scripts = HashMap::new();
        scripts.insert(
            "failTwice".to_string(),
            script(None, Some("scripts/fail-twice.rhai"), None),
        );
        // Registry file paths resolve the same way as response-level file paths.
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(
            dir.path().join("scripts").join("fail-twice.rhai"),
            "fn respond() {}",
        )
        .unwrap();
        let mut config = ImposterConfig {
            rift: Some(RiftConfig {
                scripts,
                ..Default::default()
            }),
            stubs: vec![stub_with_script(script(None, None, Some("failTwice")))],
            ..Default::default()
        };
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ConfigRelative(dir.path().to_path_buf()),
        )
        .unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("fn respond() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
    }

    /// Issue #1209: `response_script_mut` has two arms, and every other test in this module goes
    /// through `stub_with_script`, which only ever builds the `RiftScript` variant. This covers the
    /// other one — a `_rift.script` written beside an `is` response — so the `Is` arm's resolution
    /// is pinned rather than assumed to behave like its sibling.
    #[test]
    fn a_ref_resolves_on_a_script_written_beside_an_is_response() {
        let mut scripts = HashMap::new();
        scripts.insert(
            "failTwice".to_string(),
            script(Some("fn respond() {}"), None, None),
        );
        let is_with_script = Stub {
            name: None,
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::new_is(
                crate::imposter::types::IsResponse::new(
                    200,
                    Default::default(),
                    None,
                    Default::default(),
                ),
                None,
                Some(Box::new(RiftResponseExtension {
                    dataset: None,
                    conditional: None,
                    fault: None,
                    script: Some(script(None, None, Some("failTwice"))),
                    templated: false,
                    state_ops: Vec::new(),
                })),
            )],
            scenario_name: None,
            required_scenario_state: None,
            new_scenario_state: None,
            space: None,
            recorded_from: None,
            verify: None,
        };
        let mut config = ImposterConfig {
            rift: Some(RiftConfig {
                scripts,
                ..Default::default()
            }),
            stubs: vec![is_with_script],
            ..Default::default()
        };

        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();

        let StubResponse::Is {
            rift: Some(rift), ..
        } = &config.stubs[0].responses[0]
        else {
            panic!("a `_rift` block beside an `is` response must stay on the Is variant");
        };
        let resolved = rift.script.as_ref().expect("script present");
        assert_eq!(resolved.code.as_deref(), Some("fn respond() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
        assert_eq!(resolved.ref_name, None, "the ref must be consumed");
    }

    #[test]
    fn unknown_ref_is_an_error() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, None, Some("nope")))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap_err();
        assert!(matches!(err, ScriptResolveError::UnknownRef(name) if name == "nope"));
    }

    #[test]
    fn ref_to_ref_is_rejected() {
        let mut scripts = HashMap::new();
        scripts.insert("a".to_string(), script(None, None, Some("b")));
        scripts.insert("b".to_string(), script(Some("code"), None, None));
        let mut config = ImposterConfig {
            rift: Some(RiftConfig {
                scripts,
                ..Default::default()
            }),
            stubs: vec![stub_with_script(script(None, None, Some("a")))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap_err();
        assert!(matches!(err, ScriptResolveError::RefChain(name) if name == "a"));
    }

    // Issue #356 B5: an ABSOLUTE `file:` path under a `--scripts-dir` root is rejected lexically
    // (the RootDir component in `safe_join`) before any read — `/etc/passwd` never opened.
    #[test]
    fn scripts_dir_absolute_path_is_rejected_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        // A real, readable absolute target outside the root — proves the rejection is the
        // absolute-path check, not a missing file.
        let secret = dir.path().join("secret.rhai");
        std::fs::write(&secret, "SECRET").unwrap();
        let root = dir.path().join("scripts");
        std::fs::create_dir(&root).unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(
                None,
                Some(secret.to_str().unwrap()),
                None,
            ))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::ScriptsDir(root)).unwrap_err();
        assert!(
            matches!(err, ScriptResolveError::PathEscape(_)),
            "absolute path must be PathEscape, got {err:?}"
        );
        // Nothing was loaded into `code`.
        assert_eq!(extract_script(&config).code, None);
    }

    // Issue #356 B5: a symlink INSIDE the root pointing OUTSIDE it is rejected (fail-closed) and
    // its target's content is never returned — the canonicalize-before-read guard.
    #[cfg(unix)]
    #[test]
    fn scripts_dir_symlink_escape_is_rejected_without_reading() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.rhai");
        std::fs::write(&secret, "SUPER-SECRET-CONTENTS").unwrap();
        let root = dir.path().join("scripts");
        std::fs::create_dir(&root).unwrap();
        // scripts/leak.rhai -> ../secret.rhai (target is real and outside the root).
        std::os::unix::fs::symlink(&secret, root.join("leak.rhai")).unwrap();

        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("leak.rhai"), None))],
            ..Default::default()
        };
        let err = resolve_scripts(&mut config, &ScriptBaseDir::ScriptsDir(root)).unwrap_err();
        assert!(
            matches!(err, ScriptResolveError::PathEscape(_)),
            "symlink escape must be PathEscape, got {err:?}"
        );
        // The secret's content must NOT have been read into `code`.
        let resolved = extract_script(&config);
        assert_eq!(resolved.code, None);
    }

    // Issue #356 B2: the datadir base applies the SAME escape rules as `--scripts-dir` — a
    // network-authored `{port}.json` cannot read an absolute or `..`-escaping path.
    #[test]
    fn datadir_relative_rejects_absolute_and_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("secret.rhai"), "SECRET").unwrap();
        let root = dir.path().join("data");
        std::fs::create_dir(&root).unwrap();

        for bad in ["/etc/passwd", "../secret.rhai"] {
            let mut config = ImposterConfig {
                stubs: vec![stub_with_script(script(None, Some(bad), None))],
                ..Default::default()
            };
            let err = resolve_scripts(&mut config, &ScriptBaseDir::DatadirRelative(root.clone()))
                .unwrap_err();
            assert!(
                matches!(err, ScriptResolveError::PathEscape(_)),
                "datadir `{bad}` must be PathEscape, got {err:?}"
            );
            assert_eq!(extract_script(&config).code, None);
        }
    }

    #[test]
    fn datadir_relative_within_root_is_read() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("ok.rhai"), "fn respond() {}").unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("ok.rhai"), None))],
            ..Default::default()
        };
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::DatadirRelative(root.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            extract_script(&config).code.as_deref(),
            Some("fn respond() {}")
        );
    }

    // Issue #356: a `.js` file infers the "javascript" engine.
    #[test]
    fn js_file_extension_infers_javascript_engine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("script.js"),
            "function respond() { return pass(); }",
        )
        .unwrap();
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(None, Some("script.js"), None))],
            ..Default::default()
        };
        resolve_scripts(
            &mut config,
            &ScriptBaseDir::ConfigRelative(dir.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(
            extract_script(&config).engine.as_deref(),
            Some("javascript")
        );
    }

    // ---- Issue #1611: the runtime resolver behind `require()` ----

    /// A canary tree: `root/` is the confinement root, `outside/` sits beside it.
    struct Confinement {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    fn confinement() -> Confinement {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical tempdir");
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("mod.js"), "module.exports = 1;").unwrap();
        std::fs::write(root.join("lib/helper.js"), "module.exports = 2;").unwrap();
        std::fs::write(outside.join("secret.js"), "CANARY_1611").unwrap();
        Confinement {
            _dir: dir,
            root,
            outside,
        }
    }

    fn outside_err(result: Result<PathBuf, ConfinedPathError>) -> bool {
        matches!(result, Err(ConfinedPathError::Outside))
    }

    #[test]
    fn confined_resolves_a_module_in_the_root_by_relative_and_absolute_path() {
        let c = confinement();
        let canonical = c.root.join("mod.js");
        assert_eq!(
            resolve_confined(&c.root, &c.root, "mod.js").unwrap(),
            canonical
        );
        assert_eq!(
            resolve_confined(&c.root, &c.root, "./lib/../mod.js").unwrap(),
            canonical
        );
        assert_eq!(
            resolve_confined(&c.root, &c.root, canonical.to_str().unwrap()).unwrap(),
            canonical
        );
    }

    #[test]
    fn confined_resolves_a_relative_path_from_the_requiring_module_directory() {
        let c = confinement();
        assert_eq!(
            resolve_confined(&c.root, &c.root.join("lib"), "../mod.js").unwrap(),
            c.root.join("mod.js")
        );
        assert_eq!(
            resolve_confined(&c.root, &c.root.join("lib"), "helper.js").unwrap(),
            c.root.join("lib/helper.js")
        );
    }

    #[test]
    fn confined_refuses_absolute_dotdot_and_symlink_escapes() {
        let c = confinement();
        assert!(outside_err(resolve_confined(
            &c.root,
            &c.root,
            "/etc/hosts"
        )));
        assert!(outside_err(resolve_confined(
            &c.root,
            &c.root,
            "../outside/secret.js"
        )));
        assert!(outside_err(resolve_confined(
            &c.root,
            &c.root.join("lib"),
            "../../outside/secret.js"
        )));
        assert!(outside_err(resolve_confined(
            &c.root,
            &c.root,
            c.outside.join("secret.js").to_str().unwrap()
        )));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(c.outside.join("secret.js"), c.root.join("link.js"))
                .unwrap();
            std::os::unix::fs::symlink(&c.outside, c.root.join("linkdir")).unwrap();
            std::os::unix::fs::symlink(c.outside.join("gone.js"), c.root.join("dangling.js"))
                .unwrap();
            assert!(outside_err(resolve_confined(&c.root, &c.root, "link.js")));
            assert!(outside_err(resolve_confined(
                &c.root,
                &c.root,
                "linkdir/secret.js"
            )));
            assert!(outside_err(resolve_confined(
                &c.root,
                &c.root,
                "linkdir/gone.js"
            )));
            // A dangling link is a link out, not a missing file: that would say its target is gone.
            assert!(outside_err(resolve_confined(
                &c.root,
                &c.root,
                "dangling.js"
            )));
        }
    }

    #[test]
    fn confined_refuses_a_missing_and_an_existing_outside_path_alike() {
        let c = confinement();
        let present = c.outside.join("secret.js");
        let missing = c.outside.join("no-such-file.js");
        for path in [&present, &missing] {
            let err = resolve_confined(&c.root, &c.root, path.to_str().unwrap()).unwrap_err();
            assert_eq!(err.to_string(), "outside the root", "{}", path.display());
        }
    }

    #[test]
    fn confined_reports_a_missing_file_inside_the_root() {
        let c = confinement();
        assert!(matches!(
            resolve_confined(&c.root, &c.root, "lib/missing.js"),
            Err(ConfinedPathError::NotFound)
        ));
    }

    #[test]
    fn confined_accepts_either_spelling_of_a_linked_root() {
        let c = confinement();
        let parent = c.root.parent().unwrap().to_path_buf();
        #[cfg(unix)]
        {
            let alias = parent.join("alias");
            std::os::unix::fs::symlink(&c.root, &alias).unwrap();
            // Written through the alias, read from the canonical root, and the other way round.
            let through_alias = alias.join("mod.js");
            assert_eq!(
                resolve_confined(&alias, &alias, through_alias.to_str().unwrap()).unwrap(),
                c.root.join("mod.js")
            );
            assert_eq!(
                resolve_confined(&alias, &c.root.join("lib"), "helper.js").unwrap(),
                c.root.join("lib/helper.js")
            );
        }
    }

    #[test]
    fn script_file_access_for_a_network_door_is_confined_or_refused() {
        assert_eq!(
            ScriptFileAccess::network(Some(Path::new("/srv/scripts"))),
            ScriptFileAccess::Confined(PathBuf::from("/srv/scripts"))
        );
        assert_eq!(ScriptFileAccess::network(None), ScriptFileAccess::Refused);
        assert_eq!(ScriptFileAccess::default(), ScriptFileAccess::Refused);
    }

    /// A `root/` holding `data.csv`, beside an `outside/` holding `secret.csv` (issue #1617).
    fn lookup_tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = dir.path().canonicalize().expect("canonical tempdir");
        std::fs::create_dir_all(base.join("root/sub")).expect("mkdir root");
        std::fs::create_dir_all(base.join("outside")).expect("mkdir outside");
        std::fs::write(base.join("root/data.csv"), "id\n1\n").expect("write data");
        std::fs::write(base.join("outside/secret.csv"), "id\n1\n").expect("write secret");
        (dir, base)
    }

    #[test]
    fn a_confined_lookup_resolves_only_under_the_root() {
        let (_dir, base) = lookup_tree();
        let access = ScriptFileAccess::Confined(base.join("root"));
        let inside = base.join("root/data.csv");
        for path in [
            "data.csv",
            "./sub/../data.csv",
            inside.to_str().expect("utf-8"),
        ] {
            assert_eq!(
                resolve_lookup_path(path, &access).expect("inside"),
                inside,
                "{path}"
            );
        }
        let secret = base.join("outside/secret.csv");
        for path in [
            "../outside/secret.csv",
            "sub/../../outside/secret.csv",
            secret.to_str().expect("utf-8"),
            "/etc/shells",
        ] {
            let err = resolve_lookup_path(path, &access).expect_err(path);
            assert!(matches!(err, LookupFileError::Outside(_)), "{path}: {err}");
            assert!(err.refuses_admission());
            assert_eq!(
                err.to_string(),
                format!("lookup CSV path '{path}' is outside --scripts-dir")
            );
        }
        // Missing inside the root: admitted, then unreadable.
        let missing = resolve_lookup_path("later.csv", &access).expect_err("missing");
        assert!(!missing.refuses_admission(), "{missing}");
    }

    #[cfg(unix)]
    #[test]
    fn a_confined_lookup_refuses_a_link_out_of_the_root() {
        let (_dir, base) = lookup_tree();
        let access = ScriptFileAccess::Confined(base.join("root"));
        std::os::unix::fs::symlink(base.join("outside/secret.csv"), base.join("root/link.csv"))
            .expect("file link");
        std::os::unix::fs::symlink(base.join("outside"), base.join("root/dir")).expect("dir link");
        std::os::unix::fs::symlink(base.join("outside/nothing"), base.join("root/dangling.csv"))
            .expect("dangling link");
        for path in ["link.csv", "dir/secret.csv", "dangling.csv"] {
            let err = resolve_lookup_path(path, &access).expect_err(path);
            assert!(matches!(err, LookupFileError::Outside(_)), "{path}: {err}");
        }
    }

    /// The read re-resolves once the file is open, so a path that moved out of the root between
    /// resolving it and reading it is refused rather than read.
    #[cfg(unix)]
    #[test]
    fn a_confined_read_refuses_a_path_that_moved_after_it_was_resolved() {
        let (_dir, base) = lookup_tree();
        let root = base.join("root");
        let access = ScriptFileAccess::Confined(root.clone());
        std::os::unix::fs::symlink(root.join("data.csv"), root.join("swap.csv")).expect("link");
        let resolved = resolve_lookup_path("swap.csv", &access).expect("inside");
        assert_eq!(
            read_lookup_file("swap.csv", &resolved, &access).expect("reads"),
            "id\n1\n"
        );

        std::fs::remove_file(root.join("swap.csv")).expect("unlink");
        std::os::unix::fs::symlink(base.join("outside/secret.csv"), root.join("swap.csv"))
            .expect("relink");
        let err = read_lookup_file("swap.csv", &resolved, &access).expect_err("moved");
        assert!(matches!(err, LookupFileError::Outside(_)), "{err}");

        // The resolved file itself replaced by another file: not the file that was vetted.
        std::fs::remove_file(root.join("swap.csv")).expect("unlink");
        std::fs::rename(root.join("data.csv"), root.join("old.csv")).expect("rename");
        std::fs::write(root.join("data.csv"), "id\n2\n").expect("rewrite");
        std::os::unix::fs::symlink(root.join("old.csv"), root.join("swap.csv")).expect("relink");
        let err = read_lookup_file("swap.csv", &resolved, &access).expect_err("replaced");
        assert!(matches!(err, LookupFileError::Outside(_)), "{err}");
    }

    /// A FIFO under the root would block `open` until a writer came, holding a worker; a directory
    /// would reach the read. Both are refused as fast as any path out of the root.
    #[cfg(unix)]
    #[test]
    fn a_confined_read_refuses_a_fifo_and_a_directory_without_blocking() {
        let (_dir, base) = lookup_tree();
        let root = base.join("root");
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("pipe.csv"))
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo");
        let access = ScriptFileAccess::Confined(root.clone());
        for path in ["pipe.csv", "sub"] {
            let resolved = resolve_lookup_path(path, &access).expect("inside the root");
            let (tx, rx) = std::sync::mpsc::channel();
            let worker_access = access.clone();
            std::thread::spawn(move || {
                // The receiver is gone only once the test has already failed on the timeout.
                let _ = tx.send(read_lookup_file(path, &resolved, &worker_access));
            });
            let read = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{path}: the read blocked"));
            let err = read.expect_err(path);
            assert!(matches!(err, LookupFileError::Outside(_)), "{path}: {err}");
        }
    }

    #[test]
    fn a_refused_access_reads_no_lookup_and_a_trusted_one_any() {
        let (_dir, base) = lookup_tree();
        let secret = base.join("outside/secret.csv");
        let err = resolve_lookup_path("data.csv", &ScriptFileAccess::Refused).expect_err("refused");
        assert_eq!(
            err.to_string(),
            "lookup CSV path 'data.csv' cannot be read: no --scripts-dir is configured"
        );
        assert!(err.refuses_admission());

        let trusted = ScriptFileAccess::Trusted {
            base: base.join("root"),
        };
        assert_eq!(
            resolve_lookup_path(secret.to_str().expect("utf-8"), &trusted).expect("any path"),
            secret
        );
        assert_eq!(
            resolve_lookup_path("../outside/secret.csv", &trusted).expect("relative to base"),
            secret
        );
        let missing = resolve_lookup_path("nope.csv", &trusted).expect_err("missing");
        assert!(!missing.refuses_admission(), "{missing}");
    }

    fn lookup_stub(path: &str, response: &str) -> Stub {
        serde_json::from_value(serde_json::json!({
            "responses": [{
                response: if response == "fault" {
                    serde_json::json!("CONNECTION_RESET_BY_PEER")
                } else {
                    serde_json::json!({})
                },
                "_behaviors": { "lookup": {
                    "key": { "from": "path", "using": { "method": "regex", "selector": ".*" } },
                    "fromDataSource": { "csv": { "path": path, "keyColumn": "id" } },
                    "into": "${row}"
                } }
            }]
        }))
        .expect("stub")
    }

    #[test]
    fn check_lookup_paths_refuses_what_the_access_could_never_read() {
        let (_dir, base) = lookup_tree();
        let access = ScriptFileAccess::Confined(base.join("root"));
        assert_eq!(
            check_lookup_paths(&[lookup_stub("data.csv", "is")], &access),
            Ok(())
        );
        assert_eq!(
            check_lookup_paths(&[lookup_stub("later.csv", "is")], &access),
            Ok(()),
            "a file that may be written later is admitted"
        );
        assert_eq!(
            check_lookup_paths(&[lookup_stub("../outside/secret.csv", "is")], &access),
            Err("lookup CSV path '../outside/secret.csv' is outside --scripts-dir".to_string())
        );
        assert!(
            check_lookup_paths(&[lookup_stub("data.csv", "is")], &ScriptFileAccess::Refused)
                .is_err()
        );
        // A fault response's behaviors never run, so they read nothing.
        assert_eq!(
            check_lookup_paths(&[lookup_stub("/etc/shells", "fault")], &access),
            Ok(())
        );
        assert_eq!(
            check_lookup_paths(
                &[lookup_stub("/etc/shells", "is")],
                &ScriptFileAccess::trusted_cwd()
            ),
            Ok(())
        );
    }
}
