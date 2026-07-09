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
    // Resolve the named registry first so response-level `ref:` lookups see fully-resolved
    // entries (populated `code` + `engine`, never another `ref`).
    if let Some(rift) = &mut config.rift {
        for (name, script) in rift.scripts.iter_mut() {
            if script.ref_name.is_some() {
                return Err(ScriptResolveError::RefChain(name.clone()));
            }
            validate_source_count(script)?;
            resolve_leaf(script, base)?;
        }
    }

    // Snapshot the resolved registry (small, config-time only) so per-response resolution can
    // look up `ref:` targets without holding a borrow of `config.rift` across the stub loop.
    let registry: HashMap<String, RiftScriptConfig> = config
        .rift
        .as_ref()
        .map(|r| r.scripts.clone())
        .unwrap_or_default();

    resolve_stub_scripts(&mut config.stubs, &registry, base)
}

/// Resolve `_rift.script` sources in a set of stubs against an already-resolved `registry`
/// (issue #356). Used by the admin-API stub sub-resource endpoints (`POST/PUT .../stubs[...]`,
/// space stubs), which add stubs to an existing imposter and must resolve — and therefore
/// escape-check — `file:`/`ref:` at WRITE time, before persisting, exactly like whole-imposter
/// create. `registry` is the target imposter's `_rift.scripts` (its entries must themselves be
/// already resolved — `code` populated, `file`/`ref` cleared — which they are once the imposter
/// was created).
pub fn resolve_stub_scripts(
    stubs: &mut [Stub],
    registry: &HashMap<String, RiftScriptConfig>,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    for stub in stubs {
        for response in &mut stub.responses {
            if let Some(script) = response_script_mut(response) {
                resolve_response_script(script, registry, base)?;
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
        StubResponse::RiftScript { rift } => rift.script.as_mut(),
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
    resolve_leaf(script, base)
}

/// Resolve a leaf script config (`code:` or `file:`, never `ref:`): load `file:` content into
/// `code` if needed, then normalize `engine` to the effective value (explicit, else inferred
/// from the file extension, else the legacy "rhai" default).
///
/// `file` is cleared once its content is loaded into `code` — resolution is meant to collapse
/// every source down to a plain `code`/`engine` pair, so everything downstream (validation,
/// execution, the compiled-script cache) only ever sees an inline script, and a repeated
/// `source_count()` check (post-resolution) still sees exactly one source.
fn resolve_leaf(
    script: &mut RiftScriptConfig,
    base: &ScriptBaseDir,
) -> Result<(), ScriptResolveError> {
    if script.engine.is_none() {
        script.engine = Some(infer_engine(script.file.as_deref()));
    }
    if let Some(file) = script.file.take() {
        script.code = Some(read_script_file(&file, base)?);
    }
    Ok(())
}

/// Infer the engine from a `file:` path's extension; `.rhai`/`.lua`/`.js` map to their engines,
/// anything else (or no `file`, i.e. inline `code:`) falls back to the legacy "rhai" default.
fn infer_engine(file: Option<&str>) -> String {
    let ext = file
        .and_then(|f| Path::new(f).extension())
        .and_then(|e| e.to_str());
    match ext {
        Some("rhai") => "rhai",
        Some("lua") => "lua",
        Some("js") => "javascript",
        _ => "rhai",
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
            id: None,
            route_pattern: None,
            predicates: vec![],
            responses: vec![StubResponse::RiftScript {
                rift: RiftResponseExtension {
                    fault: None,
                    script: Some(script_config),
                    templated: false,
                },
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
            StubResponse::RiftScript { rift } => rift.script.as_ref().unwrap(),
            other => panic!("expected RiftScript, got {other:?}"),
        }
    }

    #[test]
    fn inline_code_resolves_with_default_engine() {
        let mut config = ImposterConfig {
            stubs: vec![stub_with_script(script(
                Some("fn should_inject() {}"),
                None,
                None,
            ))],
            ..Default::default()
        };
        resolve_scripts(&mut config, &ScriptBaseDir::Unconfigured).unwrap();
        let resolved = extract_script(&config);
        assert_eq!(resolved.code.as_deref(), Some("fn should_inject() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
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
        std::fs::write(dir.path().join("ok.rhai"), "fn should_inject() {}").unwrap();
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
        assert_eq!(resolved.code.as_deref(), Some("fn should_inject() {}"));
    }

    #[test]
    fn ref_resolves_from_registry() {
        let mut scripts = HashMap::new();
        scripts.insert(
            "failTwice".to_string(),
            script(Some("fn should_inject() {}"), None, None),
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
        assert_eq!(resolved.code.as_deref(), Some("fn should_inject() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
    }

    #[test]
    fn ref_resolves_a_file_backed_registry_entry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fail-twice.rhai"), "fn should_inject() {}").unwrap();
        let mut scripts = HashMap::new();
        scripts.insert(
            "failTwice".to_string(),
            script(None, Some("scripts/fail-twice.rhai"), None),
        );
        // Registry file paths resolve the same way as response-level file paths.
        std::fs::create_dir(dir.path().join("scripts")).unwrap();
        std::fs::write(
            dir.path().join("scripts").join("fail-twice.rhai"),
            "fn should_inject() {}",
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
        assert_eq!(resolved.code.as_deref(), Some("fn should_inject() {}"));
        assert_eq!(resolved.engine.as_deref(), Some("rhai"));
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
        std::fs::write(root.path().join("ok.rhai"), "fn should_inject() {}").unwrap();
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
            Some("fn should_inject() {}")
        );
    }

    // Issue #356: a `.js` file infers the "javascript" engine.
    #[test]
    fn js_file_extension_infers_javascript_engine() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("script.js"),
            "function should_inject() { return { inject: false }; }",
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
}
