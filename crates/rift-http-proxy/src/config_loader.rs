//! Loading imposter configs from the CLI-provided source (`--configfile` / `--datadir`), shared
//! by startup and the `POST /admin/reload` hot-reload endpoint (issue #197). Parsing is pure (no
//! running state is touched), so a parse error is returned rather than applied.

use crate::imposter::{ImposterConfig, ScriptBaseDir, resolve_scripts};
use crate::intercept_control::InterceptStartOptions;
use anyhow::Context;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Whether EJS tags that read the local filesystem are honoured; see [`rift_ejs::FileAccess`].
/// Re-exported under its old name so embedders' paths keep resolving (issue #1108 moved it).
pub use rift_ejs::FileAccess as EjsFileAccess;

/// Where the running imposters were loaded from, retained so reload can re-read the same source.
#[derive(Debug, Clone)]
pub enum ConfigSource {
    /// A single `--configfile` (Mountebank JSON/YAML, with optional EJS preprocessing).
    File { path: PathBuf, no_parse: bool },
    /// A `--datadir` of one-imposter-per-`.json` files.
    Dir(PathBuf),
}

/// Everything a config source declares: the imposters, plus the optional `intercept` block that
/// brings up the intercept listener with its rules already installed (issue #655).
#[derive(Debug, Default)]
pub struct LoadedConfig {
    pub imposters: Vec<ImposterConfig>,
    /// `None` when the document declares no `intercept` block — the overwhelmingly common case, and
    /// byte-for-byte the pre-#655 behaviour. Only the `{ "imposters": [...] }` wrapper object has
    /// somewhere to put one; a bare array and a `--datadir` never yield a block.
    pub intercept: Option<InterceptStartOptions>,
    /// `None` when the document declares no `routes` block. Same shape rule as
    /// `intercept`: only the `{ "imposters": [...] }` wrapper has somewhere to put
    /// one (issue #19).
    pub routes: Option<crate::front_door::RouteTable>,
}

/// Parse the source into imposter configs without creating any imposters. A parse error is
/// returned so the caller (startup or hot-reload) decides whether to apply the result.
///
/// Imposters only: the `intercept` block is boot-only, so `POST /admin/reload` — which goes through
/// here — keeps reloading imposters and leaves the running listener alone (issue #655). Callers that
/// need the block use [`load_configs_full`].
pub fn load_configs(source: &ConfigSource) -> anyhow::Result<Vec<ImposterConfig>> {
    load_configs_full(source).map(|loaded| loaded.imposters)
}

/// Parse the source into everything it declares, including the optional `intercept` block.
pub fn load_configs_full(source: &ConfigSource) -> anyhow::Result<LoadedConfig> {
    match source {
        ConfigSource::File { path, no_parse } => load_file(path, *no_parse),
        // A datadir is a directory of imposter documents with no wrapper, so it has
        // nowhere to declare either an intercept listener or a route table.
        ConfigSource::Dir(dir) => load_dir(dir).map(|imposters| LoadedConfig {
            imposters,
            intercept: None,
            routes: None,
        }),
    }
}

fn load_file(path: &Path, no_parse: bool) -> anyhow::Result<LoadedConfig> {
    let raw = std::fs::read_to_string(path)?;
    let (content, unset_env) = if no_parse {
        (raw, Vec::new())
    } else {
        let rendered = render_ejs(&raw, path, EjsFileAccess::Allowed)?;
        (rendered.text, rendered.unset_env)
    };

    // `_rift.script` `file:`/`ref:` sources (issue #356) resolve relative to the config file's
    // own directory, so a parse error and a resolve error are both surfaced up front, and
    // hot-reload (which re-runs this loader) automatically picks up edits to referenced scripts.
    let base = ScriptBaseDir::ConfigRelative(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    );
    name_unset_env_on_failure(parse_document(&content, &base), &unset_env)
}

/// Parse a document fetched from a source that is not the local filesystem (U-12's `https:`
/// built-in, and any third-party [`crate::sources::ImposterSource`]).
///
/// Format sniffing, the wrapper/single/array shapes, and the `intercept`/`routes` block rules are
/// the *same* code the `--configfile` path runs, so a document behaves identically whichever
/// source delivered it. The two deliberate differences are both fail-closed, and both exist
/// because a remote document's author is not someone who already has local access:
/// - EJS `include`/`stringify` are refused ([`EjsFileAccess::Denied`]);
/// - `_rift.script` `file:` references are refused ([`ScriptBaseDir::Unconfigured`]).
///
/// `uri` is used only to name the source in error messages.
pub fn parse_remote_document(content: &str, uri: &str) -> anyhow::Result<LoadedConfig> {
    let rendered = render_ejs(content, Path::new(uri), EjsFileAccess::Denied)?;
    name_unset_env_on_failure(
        parse_document(&rendered.text, &ScriptBaseDir::Unconfigured),
        &rendered.unset_env,
    )
}

/// The shared parse path: format sniffing, the Mountebank document shapes, and script resolution
/// against whatever base the caller's source implies. Everything above this function differs per
/// source (bytes off a disk, bytes off a socket); everything below it must not.
fn parse_document(content: &str, base: &ScriptBaseDir) -> anyhow::Result<LoadedConfig> {
    let trimmed = content.trim_start();
    let mut intercept = None;
    let mut routes = None;
    let mut configs: Vec<ImposterConfig> = if trimmed.starts_with('{') {
        // Single imposter, or a `{ "imposters": [...] }` wrapper (Mountebank format).
        let value: serde_json::Value = serde_json::from_str(content)?;
        match value.get("imposters") {
            Some(imposters) => {
                // Only the wrapper carries siblings, so this is the one shape with somewhere to
                // declare an intercept listener (issue #655). `InterceptStartOptions` is
                // `deny_unknown_fields`, so a typo here is a startup error rather than a config
                // block that silently does nothing.
                intercept = value
                    .get("intercept")
                    .map(|block| serde_json::from_value(block.clone()))
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("invalid `intercept` block: {e}"))?;
                // `returnCaKey` asks for the generated CA key in the start *response*. A config file
                // has no response to put it in, so the key was minted and dropped (issue #1152).
                if intercept
                    .as_ref()
                    .is_some_and(|block: &InterceptStartOptions| block.return_ca_key == Some(true))
                {
                    anyhow::bail!(
                        "invalid `intercept` block: `returnCaKey` cannot be honoured from a config \
                         file, because there is no response to return the key in. Start the \
                         listener with `POST /intercept` to receive a generated CA key, or supply \
                         `caCertPath`/`caKeyPath`."
                    );
                }
                // Same wrapper-only rule, same reasoning, for the front door's route
                // table (issue #19). Validated here rather than at first request: a
                // table that cannot route is a startup error, not a runtime surprise.
                routes = value
                    .get("routes")
                    .map(|block| {
                        serde_json::from_value::<crate::front_door::RouteTable>(block.clone())
                    })
                    .transpose()
                    .map_err(|e| anyhow::anyhow!("invalid `routes` block: {e}"))?;
                if let Some(table) = &routes {
                    table
                        .validate()
                        .map_err(|e| anyhow::anyhow!("invalid `routes` block: {e}"))?;
                }
                serde_json::from_value(imposters.clone())?
            }
            // A single-imposter document has no `imposters` key, so it has no wrapper to carry a
            // listener declaration — and `ImposterConfig` ignores unknown fields, so an `intercept`
            // key here would be dropped without a word: no listener, no rule, no diagnostic, and a
            // green boot (issue #655). Refuse instead; the remedy is one line of JSON.
            None if value.get("routes").is_some() => anyhow::bail!(
                "a `routes` block is only read from the `{{\"imposters\": [...], \"routes\": [...]}}` \
                 wrapper form, but this document has no `imposters` key, so the block would be \
                 ignored. Wrap the imposter in `\"imposters\": [ ... ]` (use `[]` if the file \
                 declares none)."
            ),
            None if value.get("intercept").is_some() => anyhow::bail!(
                "an `intercept` block is only read from the `{{\"imposters\": [...], \"intercept\": {{...}}}}` \
                 wrapper form, but this document has no `imposters` key, so the block would be ignored. \
                 Wrap the imposter in `\"imposters\": [ ... ]` (use `[]` if the file declares none)."
            ),
            None => vec![serde_json::from_value(value)?],
        }
    } else if trimmed.starts_with('[') {
        serde_json::from_str(content)?
    } else {
        serde_yaml::from_str(content)?
    };

    for config in &mut configs {
        resolve_scripts(config, base)?;
    }
    Ok(LoadedConfig {
        imposters: configs,
        intercept,
        routes,
    })
}

/// Why a datadir file whose imposter names no port (absent, or `0`) is refused (issue #1125). The
/// directory is keyed by port: such an imposter would be created on an auto-assigned port and written
/// again under that port on every load, so copies accumulate.
pub(crate) const DATADIR_PORT_LESS: &str = "declares no port; a datadir file must declare its port";

/// Why a datadir file cannot be loaded as the imposter it contains, or `None` when it can.
///
/// The directory is keyed by port: rift writes an imposter to `<port>.json` and nowhere else. A file
/// that names no port (issue #1125), or is named after anything but the port it declares (issue
/// #1128), would be created and then written again as `<port>.json` beside the original, leaving two
/// files for one imposter.
pub(crate) fn datadir_file_problem(path: &Path, config: &ImposterConfig) -> Option<String> {
    let Some(port) = config.explicit_port() else {
        return Some(DATADIR_PORT_LESS.to_string());
    };
    let expected = format!("{port}.json");
    let name = path.file_name().map(|n| n.to_string_lossy());
    match name {
        Some(name) if name == expected.as_str() => None,
        Some(name) => Some(format!(
            "declares port {port} but is named {name}; a datadir file must be named {expected}"
        )),
        None => Some(format!("a datadir file must be named {expected}")),
    }
}

fn load_dir(dir: &Path) -> anyhow::Result<Vec<ImposterConfig>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Datadir `{port}.json` files can be network-authored (a stub POSTed through the admin API is
    // persisted here), so `file:` references are escape-checked — an absolute path or a `..`
    // escape is rejected, never read (issue #356 B1/B2 defense-in-depth).
    let base = ScriptBaseDir::DatadirRelative(dir.to_path_buf());
    let mut configs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            // Named, because a reload refuses the whole set on one bad file and the operator has to
            // find it among the others (issue #1122).
            let in_file = || format!("datadir file {}", path.display());
            let content = std::fs::read_to_string(&path).with_context(in_file)?;
            let mut config: ImposterConfig =
                serde_json::from_str(&content).with_context(in_file)?;
            if let Some(problem) = datadir_file_problem(&path, &config) {
                return Err(anyhow::anyhow!(problem)).with_context(in_file);
            }
            resolve_scripts(&mut config, &base).with_context(in_file)?;
            configs.push(config);
        }
    }
    Ok(configs)
}

/// Pre-process EJS tokens in a config file before JSON/YAML parsing: the tag subset and every
/// refusal live in [`rift_ejs::render`], shared with `rift-lint` (issue #1108). A variable that
/// cannot be substituted still renders empty (or its default), and is logged as a warning here
/// (issue #1116); the warning is emitted from this crate so its own `traced_test`s can see it.
fn render_ejs(
    content: &str,
    config_path: &Path,
    file_access: EjsFileAccess,
) -> anyhow::Result<rift_ejs::Rendered> {
    let rendered = rift_ejs::render(content, config_path, file_access)?;
    for problem in &rendered.unset_env {
        warn!(variable = %problem.name, place = %problem.place, "EJS: {}", problem.describe());
    }
    Ok(rendered)
}

/// The rendered text alone, for the preprocessing tests that assert on it.
#[cfg(test)]
fn preprocess_ejs(
    content: &str,
    config_path: &Path,
    file_access: EjsFileAccess,
) -> anyhow::Result<String> {
    Ok(render_ejs(content, config_path, file_access)?.text)
}

/// When the rendered document fails to parse, name the variables that rendered empty: the parse
/// error alone gives a line and column in text the author never wrote, which is exactly what an
/// unset `"port": <%= process.env.PORT %>` produces (issue #1116).
fn name_unset_env_on_failure(
    parsed: anyhow::Result<LoadedConfig>,
    unset_env: &[rift_ejs::UnsetEnv],
) -> anyhow::Result<LoadedConfig> {
    if unset_env.is_empty() {
        return parsed;
    }
    parsed.with_context(|| {
        let problems: Vec<String> = unset_env.iter().map(rift_ejs::UnsetEnv::describe).collect();
        format!(
            "the document failed to load after EJS rendering; {}",
            problems.join("; ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    /// Issue #1088 pins this: rift-lint's E047 says a non-integer `port` is refused at load. An
    /// integral float is no exception — serde's `u16` visitor has no float arm.
    #[test]
    fn a_non_integer_port_is_refused_at_load() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("float.json", r#"{"port": 3000.0, "protocol": "http"}"#),
            ("string.json", r#"{"port": "3000", "protocol": "http"}"#),
            ("float.yaml", "- port: 3000.5\n  protocol: http\n"),
        ] {
            let path = write(dir.path(), name, body);
            let err = load_configs(&ConfigSource::File {
                path,
                no_parse: false,
            })
            .expect_err("a non-integer port must not load");
            assert!(
                format!("{err:#}").contains("expected u16"),
                "{name}: {err:#}"
            );
        }
    }

    /// ...while an explicit `null` loads as an absent port (auto-assigned), which is why the lint
    /// reports it as E003 rather than E047.
    #[test]
    fn a_null_port_loads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "null.json",
            r#"{"port": null, "protocol": "http"}"#,
        );
        let configs = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, None);
    }

    #[test]
    fn parses_single_wrapper_and_array_files() {
        let dir = tempfile::tempdir().unwrap();
        let single = write(
            dir.path(),
            "single.json",
            r#"{"port":8000,"protocol":"http"}"#,
        );
        let wrapper = write(
            dir.path(),
            "wrap.json",
            r#"{"imposters":[{"port":8001,"protocol":"http"},{"port":8002,"protocol":"http"}]}"#,
        );
        let array = write(
            dir.path(),
            "arr.json",
            r#"[{"port":8003,"protocol":"http"}]"#,
        );

        let one = load_configs(&ConfigSource::File {
            path: single,
            no_parse: false,
        })
        .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].port, Some(8000));
        let two = load_configs(&ConfigSource::File {
            path: wrapper,
            no_parse: false,
        })
        .unwrap();
        assert_eq!(two.len(), 2);
        let arr = load_configs(&ConfigSource::File {
            path: array,
            no_parse: false,
        })
        .unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn dir_loads_all_json_and_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "8100.json",
            r#"{"port":8100,"protocol":"http"}"#,
        );
        write(
            dir.path(),
            "8101.json",
            r#"{"port":8101,"protocol":"http"}"#,
        );
        write(dir.path(), "notes.txt", "ignored"); // non-json skipped
        let configs = load_configs(&ConfigSource::Dir(dir.path().to_path_buf())).unwrap();
        assert_eq!(configs.len(), 2);

        write(dir.path(), "bad.json", "not json");
        assert!(
            load_configs(&ConfigSource::Dir(dir.path().to_path_buf())).is_err(),
            "a malformed file makes the whole reload fail (no partial apply)"
        );
    }

    // Issue #356 B1 (security regression): a persisted datadir `{port}.json` carrying an absolute
    // or `..`-escaping `_rift.script.file:` is REJECTED on load — never read. This is the proof
    // that a stub POSTed through the admin API and persisted here cannot turn a later
    // reload/restart into an arbitrary file read (`/etc/passwd`).
    #[test]
    fn datadir_rejects_escaping_file_script_without_reading() {
        for bad in ["/etc/passwd", "../secret.rhai"] {
            let dir = tempfile::tempdir().unwrap();
            // A real secret adjacent to the datadir that a naive resolver would read.
            std::fs::write(dir.path().join("secret.rhai"), "SUPER-SECRET").unwrap();
            let datadir = dir.path().join("data");
            std::fs::create_dir(&datadir).unwrap();
            let cfg = format!(
                r#"{{"port":8300,"protocol":"http","stubs":[{{"responses":[{{"_rift":{{"script":{{"file":"{bad}"}}}}}}]}}]}}"#
            );
            write(&datadir, "8300.json", &cfg);

            let result = load_configs(&ConfigSource::Dir(datadir));
            let err = result.expect_err("escaping datadir file: must be rejected");
            // The whole chain: the outermost context names the datadir file (issue #1122).
            let msg = format!("{err:#}");
            assert!(
                msg.contains("escapes"),
                "datadir `{bad}` should be a path-escape error, got: {msg}"
            );
            assert!(
                !msg.contains("SUPER-SECRET"),
                "the secret's content must never appear (it must not be read): {msg}"
            );
        }
    }

    // ===== Optional `intercept` block (issue #655) =====

    const WITH_INTERCEPT: &str = r#"{
        "imposters": [{"port":4545,"protocol":"http"}],
        "intercept": {
            "host": "0.0.0.0",
            "port": 8080,
            "auth": {"username":"ci","password":"s3cr3t"},
            "rules": [{"host":"cdn.example.com","action":{"forward":{"port":4545}}}]
        }
    }"#;

    /// AC2: the block is read from the same wrapper object that already carries `imposters`.
    /// The wrapper form carries a `routes` table through to the caller, validated
    /// at load time so a table that cannot route fails the boot rather than the
    /// first request.
    #[test]
    fn load_configs_full_reads_and_validates_a_routes_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "cfg.json",
            r#"{"imposters": [{"port": 4545, "protocol": "http"}],
                "routes": {"routes": [
                    {"id": "pay", "match": {"host": "payments.test"}, "target": {"port": 4545}}
                ]}}"#,
        );
        let loaded = load_configs_full(&ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        })
        .unwrap();
        let table = loaded.routes.expect("routes block is read");
        assert_eq!(table.routes.len(), 1);
        assert_eq!(table.routes[0].id, "pay");
        assert_eq!(table.routes[0].target.port, 4545);

        let bad = write(
            dir.path(),
            "bad.json",
            r#"{"imposters": [],
                "routes": {"routes": [
                    {"id": "dup", "match": {}, "target": {"port": 1}},
                    {"id": "dup", "match": {}, "target": {"port": 2}}
                ]}}"#,
        );
        let err = load_configs_full(&ConfigSource::File {
            path: bad,
            no_parse: false,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("routes"), "error should name the block: {err}");
    }

    /// A `routes` block on a document with no `imposters` wrapper would be
    /// silently dropped — `ImposterConfig` ignores unknown fields — leaving no
    /// route table, no diagnostic, and a green boot. Same trap as #655's
    /// `intercept` block, same refusal.
    #[test]
    fn a_routes_block_outside_the_wrapper_is_refused_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "single.json",
            r#"{"port": 4545, "protocol": "http",
                "routes": {"routes": [
                    {"id": "pay", "match": {}, "target": {"port": 4545}}
                ]}}"#,
        );
        let err = load_configs_full(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`routes` block") && err.contains("imposters"),
            "the refusal must say what was wrong and how to fix it: {err}"
        );
    }

    #[test]
    fn load_configs_full_reads_intercept_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "cfg.json", WITH_INTERCEPT);
        let loaded = load_configs_full(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .unwrap();

        assert_eq!(loaded.imposters.len(), 1);
        assert_eq!(loaded.imposters[0].port, Some(4545));
        let intercept = loaded.intercept.expect("the intercept block is read");
        assert_eq!(intercept.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(intercept.port, Some(8080));
        assert_eq!(intercept.rules.len(), 1);
        assert_eq!(intercept.rules[0].host.as_deref(), Some("cdn.example.com"));
        // Issue #878: the config-file door had no coverage of `auth` at all — it had never been
        // proven to parse, let alone reach the listener.
        let auth = intercept.auth.expect("the block's auth is read");
        assert_eq!(auth.username, "ci");
        assert_eq!(auth.password, "s3cr3t");
    }

    /// AC2: absent block → exactly today's behaviour, imposters untouched.
    #[test]
    fn load_configs_full_without_intercept_block_is_none() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            (
                "wrap.json",
                r#"{"imposters":[{"port":8001,"protocol":"http"}]}"#,
            ),
            ("single.json", r#"{"port":8000,"protocol":"http"}"#),
            ("arr.json", r#"[{"port":8003,"protocol":"http"}]"#),
        ] {
            let path = write(dir.path(), name, body);
            let loaded = load_configs_full(&ConfigSource::File {
                path,
                no_parse: false,
            })
            .unwrap();
            assert_eq!(loaded.imposters.len(), 1, "{name}: imposters still load");
            assert!(
                loaded.intercept.is_none(),
                "{name}: no block means no intercept"
            );
        }
    }

    // Issue #1128: the datadir is keyed by port, so a file must be named after the port it declares.
    #[test]
    fn a_datadir_file_not_named_after_its_port_refuses_the_load() {
        for name in ["foo.json", "08102.json", "imposter-8102.json"] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), name, r#"{"port":8102,"protocol":"http"}"#);
            let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
                .expect_err("a misnamed datadir file refuses the load");
            let message = format!("{err:#}");
            assert!(
                message.contains(name)
                    && message.contains(&format!(
                        "declares port 8102 but is named {name}; a datadir file must be named 8102.json"
                    )),
                "got: {message}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "8102.json",
            r#"{"port":8102,"protocol":"http"}"#,
        );
        let configs = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
            .expect("a file named after its port loads");
        assert_eq!(configs.len(), 1);
    }

    // Issue #1125: a reload refuses a datadir file that names no port, whether `port` is absent or 0.
    #[test]
    fn a_port_less_datadir_file_refuses_the_load() {
        for (name, body) in [
            ("0a.json", r#"{"protocol":"http"}"#),
            ("0b.json", r#"{"port":0,"protocol":"http"}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(
                dir.path(),
                "8100.json",
                r#"{"port":8100,"protocol":"http"}"#,
            );
            write(dir.path(), name, body);
            let err = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
                .expect_err("a port-less datadir file refuses the load");
            let message = format!("{err:#}");
            assert!(
                message.contains(name) && message.contains("declares no port"),
                "got: {message}"
            );
        }
    }

    /// A `--datadir` has no wrapper object to carry a block.
    #[test]
    fn load_configs_full_from_dir_has_no_intercept_block() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "8100.json",
            r#"{"port":8100,"protocol":"http"}"#,
        );
        let loaded = load_configs_full(&ConfigSource::Dir(dir.path().to_path_buf())).unwrap();
        assert_eq!(loaded.imposters.len(), 1);
        assert!(loaded.intercept.is_none());
    }

    /// AC5: `POST /admin/reload` goes through `load_configs`, which must keep returning imposters
    /// only — the block is boot-only, and a config carrying one must still reload its imposters
    /// rather than erroring.
    #[test]
    fn load_configs_returns_imposters_only_for_a_config_with_a_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "cfg.json", WITH_INTERCEPT);
        let configs = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect("a config with an intercept block still reloads its imposters");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].port, Some(4545));
    }

    /// The block is `InterceptStartOptions`, which is `deny_unknown_fields` — a typo is a loud
    /// startup error, not a silently-ignored listener.
    #[test]
    fn misspelled_intercept_field_is_a_load_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "typo.json",
            r#"{"imposters":[],"intercept":{"prot":8080}}"#,
        );
        let err = load_configs_full(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("a misspelled block field must fail the load");
        let msg = err.to_string();
        assert!(
            msg.contains("prot") || msg.contains("unknown field"),
            "the error must name the offending field: {msg}"
        );
    }

    /// An `intercept` key outside the wrapper form must be a loud error, never a silent no-op:
    /// `ImposterConfig` ignores unknown fields, so the block would otherwise vanish and rift would
    /// boot green with no listener. Both single-imposter spellings are covered — with and without
    /// other imposter fields — because the intercept-only document (`{"intercept": {...}}`) also
    /// parses as a *default* `ImposterConfig`, which would additionally conjure a phantom
    /// auto-assigned-port imposter out of a file that declares none.
    #[test]
    fn intercept_block_outside_the_wrapper_form_is_a_loud_error() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            (
                "single_plus_block.json",
                r#"{"port":8000,"protocol":"http","intercept":{"port":8080,"rules":[]}}"#,
            ),
            (
                "block_only.json",
                r#"{"intercept":{"port":8080,"rules":[]}}"#,
            ),
        ] {
            let path = write(dir.path(), name, body);
            let err = load_configs_full(&ConfigSource::File {
                path,
                no_parse: false,
            })
            .expect_err("{name}: a block outside the wrapper must not be silently dropped");
            let msg = err.to_string();
            assert!(
                msg.contains("imposters"),
                "{name}: the error must name the wrapper key that fixes it: {msg}"
            );
        }
    }

    /// The guard above must not fire on documents that never mentioned `intercept` — the
    /// single-imposter shape stays exactly as it was.
    #[test]
    fn single_imposter_without_a_block_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "single.json",
            r#"{"port":8000,"protocol":"http"}"#,
        );
        let loaded = load_configs_full(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect("a plain single-imposter config is unaffected");
        assert_eq!(loaded.imposters.len(), 1);
        assert_eq!(loaded.imposters[0].port, Some(8000));
        assert!(loaded.intercept.is_none());
    }

    /// EJS runs before parsing, so the block gets env substitution like the rest of the file.
    #[test]
    fn intercept_block_supports_ejs_env_substitution() {
        unsafe { std::env::set_var("RIFT_TEST_655_HOST", "flags.example.com") };
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "ejs.json",
            r#"{"imposters":[],"intercept":{"rules":[{"host":"<%= process.env.RIFT_TEST_655_HOST %>","action":{"forward":{"port":4545}}}]}}"#,
        );
        let loaded = load_configs_full(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .unwrap();
        assert_eq!(
            loaded.intercept.expect("block").rules[0].host.as_deref(),
            Some("flags.example.com")
        );
        unsafe { std::env::remove_var("RIFT_TEST_655_HOST") };
    }

    #[test]
    fn parse_error_is_returned_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad.json", "{ not valid json");
        assert!(
            load_configs(&ConfigSource::File {
                path: bad,
                no_parse: false
            })
            .is_err()
        );
    }

    // EJS configfile pre-processing (relocated from main.rs with preprocess_ejs in issue #197)

    #[test]
    fn test_ejs_no_tokens_passthrough() {
        let content = r#"{"imposters": []}"#;
        let path = PathBuf::from("config.json");
        assert_eq!(
            preprocess_ejs(content, &path, EjsFileAccess::Allowed).unwrap(),
            content
        );
    }

    // Issue #1116: an unset variable with no default renders empty, and the engine used to say
    // nothing. It is now a warning at load, naming the variable and the tag's place.
    #[test]
    #[tracing_test::traced_test]
    fn an_unset_variable_is_warned_at_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters": [{"port": 4545, "protocol": "http", "stubs": [{"responses": [
                {"is": {"body": "<%= process.env.RIFT_TEST_1116_NEVER_SET %>"}}]}]}]}"#,
        )
        .expect("write config");
        let configs = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect("an empty body is a valid config");
        assert_eq!(configs.len(), 1);
        assert!(logs_contain("RIFT_TEST_1116_NEVER_SET"));
        assert!(logs_contain("imposters.json:2"));
    }

    // When the empty value then breaks the document, the parse error alone names a line and column
    // and not the variable; the load error now names it.
    #[test]
    fn a_parse_error_after_an_unset_variable_names_the_variable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters": [{"port": <%= process.env.RIFT_TEST_1116_NEVER_SET_PORT %>, "protocol": "http", "stubs": []}]}"#,
        )
        .expect("write config");
        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("an empty port is not JSON");
        let message = format!("{err:#}");
        assert!(
            message.contains("RIFT_TEST_1116_NEVER_SET_PORT"),
            "{message}"
        );
    }

    // The same wiring on the remote-document path, which has its own call site.
    #[test]
    #[tracing_test::traced_test]
    fn a_remote_document_warns_and_names_an_unset_variable() {
        let loaded = parse_remote_document(
            r#"{"port": 4545, "protocol": "http", "stubs": [{"responses": [{"is": {"body": "<%= process.env.RIFT_TEST_1116_REMOTE_NEVER_SET %>"}}]}]}"#,
            "https://h/imposters.json",
        )
        .expect("an empty body is a valid document");
        assert_eq!(loaded.imposters.len(), 1);
        assert!(logs_contain("RIFT_TEST_1116_REMOTE_NEVER_SET"));

        let err = parse_remote_document(
            r#"{"port": <%= process.env.RIFT_TEST_1116_REMOTE_NEVER_SET_PORT %>, "protocol": "http", "stubs": []}"#,
            "https://h/imposters.json",
        )
        .expect_err("an empty port is not JSON");
        assert!(
            format!("{err:#}").contains("RIFT_TEST_1116_REMOTE_NEVER_SET_PORT"),
            "{err:#}"
        );
    }

    // The note is added only when a variable rendered empty; a plain parse error, or one in a file
    // whose variables all had defaults, is left as it was, and nothing is warned.
    #[test]
    #[tracing_test::traced_test]
    fn a_parse_error_without_an_unset_variable_is_not_annotated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters": [{"port": <%= process.env.RIFT_TEST_1116_DEFAULTED || '4545' %>,, "protocol": "http", "stubs": []}]}"#,
        )
        .expect("write config");
        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("a double comma is not JSON");
        assert!(!format!("{err:#}").contains("EJS rendering"), "{err:#}");
        assert!(!logs_contain("RIFT_TEST_1116_DEFAULTED"));
    }

    #[test]
    fn test_ejs_env_var_substitution() {
        unsafe { std::env::set_var("RIFT_TEST_HOST", "myhost") };
        let content = r#"{"body": "<%= process.env.RIFT_TEST_HOST %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path, EjsFileAccess::Allowed).unwrap();
        assert_eq!(result, r#"{"body": "myhost"}"#);
        unsafe { std::env::remove_var("RIFT_TEST_HOST") };
    }

    #[test]
    fn test_ejs_env_var_with_default() {
        unsafe { std::env::remove_var("RIFT_TEST_UNSET_VAR") };
        let content = r#"{"port": <%= process.env.RIFT_TEST_UNSET_VAR || '4545' %>}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path, EjsFileAccess::Allowed).unwrap();
        assert_eq!(result, r#"{"port": 4545}"#);
    }

    #[test]
    fn test_ejs_env_var_present_overrides_default() {
        unsafe { std::env::set_var("RIFT_TEST_PORT", "8080") };
        let content = r#"{"port": <%= process.env.RIFT_TEST_PORT || '4545' %>}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path, EjsFileAccess::Allowed).unwrap();
        assert_eq!(result, r#"{"port": 8080}"#);
        unsafe { std::env::remove_var("RIFT_TEST_PORT") };
    }

    /// Issue #1092: the env-var port example in `docs/mountebank/imposters.md`, loaded for real.
    /// The preprocessor tests above only compare text, which is how a quoted port that could never
    /// boot sat in the docs.
    #[test]
    fn the_documented_env_var_port_form_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "imposters.json",
            r#"{"imposters": [{"port": <%= process.env.RIFT_TEST_DOC_PORT || '4545' %>, "protocol": "http", "stubs": []}]}"#,
        );
        let source = ConfigSource::File {
            path,
            no_parse: false,
        };

        unsafe { std::env::remove_var("RIFT_TEST_DOC_PORT") };
        assert_eq!(load_configs(&source).unwrap()[0].port, Some(4545));

        unsafe { std::env::set_var("RIFT_TEST_DOC_PORT", "8080") };
        let with_var = load_configs(&source);
        unsafe { std::env::remove_var("RIFT_TEST_DOC_PORT") };
        assert_eq!(with_var.unwrap()[0].port, Some(8080));
    }

    /// Issue #1092: the tag the TLS docs used before. It matches neither `include` nor `stringify`;
    /// it used to be stripped, leaving the key silently empty, and is now refused (issue #1095).
    #[test]
    fn a_call_style_include_tag_is_refused_not_stripped() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "server.key",
            "-----BEGIN KEY-----\nabc\n-----END KEY-----\n",
        );
        let path = dir.path().join("cfg.json");
        let content = r#"{"key": "<%- include('server.key') %>"}"#;
        let err = preprocess_ejs(content, &path, EjsFileAccess::Allowed)
            .expect_err("an unsupported tag must fail the load")
            .to_string();
        assert!(
            err.contains("`<%- include('server.key') %>`") && err.contains("cfg.json:1"),
            "the error must name the tag and where it is: {err}"
        );
        let content = r#"{"key": "<%- stringify('server.key') %>"}"#;
        assert_eq!(
            preprocess_ejs(content, &path, EjsFileAccess::Allowed).unwrap(),
            r#"{"key": "-----BEGIN KEY-----\nabc\n-----END KEY-----\n"}"#
        );
    }

    #[test]
    fn test_ejs_include_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("partial.json"), r#"{"key": "value"}"#).unwrap();
        let content = r#"<% include 'partial.json' %>"#.to_string();
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(&content, &config_path, EjsFileAccess::Allowed).unwrap();
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_ejs_include_unquoted_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("partial.json"), r#"[1,2,3]"#).unwrap();
        let content = r#"<% include partial.json %>"#;
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(content, &config_path, EjsFileAccess::Allowed).unwrap();
        assert_eq!(result, "[1,2,3]");
    }

    #[test]
    fn test_ejs_missing_include_is_fatal_error() {
        let content = r#"<% include 'nonexistent.json' %>"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path, EjsFileAccess::Allowed);
        assert!(result.is_err(), "missing include file should return Err");
        assert!(
            result.unwrap_err().to_string().contains("nonexistent.json"),
            "error message should name the missing file"
        );
    }

    // Issue #355 Item 7: `<%- stringify('path') %>` inlines a file's contents as a JSON-string-
    // safe body, producing the same parsed config as writing the script inline.
    #[test]
    fn ejs_stringify_inlines_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("inject.js"),
            "function (config) {\n  return { statusCode: 200, body: 'hi' };\n}",
        )
        .unwrap();
        let content = r#"{"port": 9000, "protocol": "http", "stubs": [{"responses": [{"inject": "<%- stringify('inject.js') %>"}]}]}"#;
        let config_path = dir.path().join("config.ejs");
        let processed = preprocess_ejs(content, &config_path, EjsFileAccess::Allowed).unwrap();

        // The substituted content must keep the surrounding JSON valid.
        let processed_value: serde_json::Value =
            serde_json::from_str(&processed).expect("stringify output must stay valid JSON");

        let inlined = serde_json::json!({
            "port": 9000, "protocol": "http",
            "stubs": [{"responses": [{
                "inject": "function (config) {\n  return { statusCode: 200, body: 'hi' };\n}"
            }]}]
        });
        assert_eq!(
            processed_value["stubs"][0]["responses"][0]["inject"],
            inlined["stubs"][0]["responses"][0]["inject"],
            "stringify output must match the inline-string equivalent"
        );

        let processed_config: ImposterConfig = serde_json::from_value(processed_value).unwrap();
        let inlined_config: ImposterConfig = serde_json::from_value(inlined).unwrap();
        assert_eq!(
            serde_json::to_value(&processed_config).unwrap(),
            serde_json::to_value(&inlined_config).unwrap(),
            "the stringify'd config must parse identically to the inlined-string version"
        );
    }

    #[test]
    fn ejs_stringify_missing_file_is_fatal_error() {
        let content = r#"{"inject": "<%- stringify('nope-355.js') %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path, EjsFileAccess::Allowed);
        assert!(result.is_err(), "missing stringify file should return Err");
        assert!(
            result.unwrap_err().to_string().contains("nope-355.js"),
            "error message should name the missing file"
        );
    }

    /// Issue #1095: the refusal message, in full, for an expression tag on a later line.
    #[test]
    fn an_unsupported_expression_is_refused_naming_tag_file_and_line() {
        let content = "{\n  \"imposters\": [\n    {\"port\": <%= port || 4545 %>, \"protocol\": \"http\"}\n  ]\n}";
        let path = PathBuf::from("dir/imposters.json");
        let err = preprocess_ejs(content, &path, EjsFileAccess::Allowed)
            .expect_err("an unsupported expression must fail the load")
            .to_string();
        assert_eq!(
            err,
            "unsupported EJS tag `<%= port || 4545 %>` at dir/imposters.json:3, so the file was \
             not loaded. Only `<%= process.env.VAR %>`, `<%= process.env.VAR || 'default' %>`, \
             `<% include 'file' %>` and `<%- stringify('file') %>` are evaluated. If the tag is \
             meant literally, load the file with --no-parse (a --configfile or file: source)."
        );
    }

    /// Every tag shape that used to be blanked or stripped is refused, and each refusal names the
    /// tag it found — including the ones the old regexes never matched and left for the JSON parser.
    #[test]
    fn every_unsupported_tag_shape_is_refused_by_name() {
        let path = PathBuf::from("config.json");
        for (content, tag) in [
            (
                r#"{"a": 1<% for (var i=0;i<3;i++) { %><% } %>}"#,
                "`<% for (var i=0;i<3;i++) { %>`",
            ),
            (
                "{\"a\": 1<% if (x) {\n  y();\n} %>}",
                "`<% if (x) {\n  y();\n} %>`",
            ),
            (r#"{"a": "<%- request.path %>"}"#, "`<%- request.path %>`"),
            (r#"{"a": "<%# a comment %>"}"#, "`<%# a comment %>`"),
            (r#"{"a": "<%% literal %>"}"#, "`<%% literal %>`"),
            (
                "{\"a\": <%= process.env\n.HOME %>}",
                "`<%= process.env\n.HOME %>`",
            ),
            (
                r#"{"a": "<%= process.env.lower-case %>"}"#,
                "`<%= process.env.lower-case %>`",
            ),
        ] {
            let err = preprocess_ejs(content, &path, EjsFileAccess::Allowed)
                .expect_err(content)
                .to_string();
            assert!(
                err.starts_with(&format!("unsupported EJS tag {tag} at config.json:")),
                "for {content:?}: {err}"
            );
        }
    }

    /// Whitespace around the body may include newlines, so a tag laid out over three lines is still
    /// the supported expression, substituted like the one-line form.
    #[test]
    fn a_supported_expression_padded_with_newlines_is_substituted() {
        unsafe { std::env::set_var("RIFT_TEST_1095_PAD", "padded") };
        let content = "{\"a\": \"<%=\n  process.env.RIFT_TEST_1095_PAD\n%>\"}";
        let result = preprocess_ejs(content, &PathBuf::from("c.json"), EjsFileAccess::Allowed);
        unsafe { std::env::remove_var("RIFT_TEST_1095_PAD") };
        assert_eq!(result.unwrap(), r#"{"a": "padded"}"#);
    }

    #[test]
    fn an_unterminated_tag_is_refused() {
        let content = r#"{"a": "<% never closed"}"#;
        let err = preprocess_ejs(
            content,
            &PathBuf::from("config.json"),
            EjsFileAccess::Allowed,
        )
        .expect_err("an unterminated tag must fail the load")
        .to_string();
        assert!(
            err.starts_with(
                "unsupported EJS tag `<% never closed\"}` (no closing `%>`) at config.json:1"
            ),
            "{err}"
        );
    }

    /// A supported tag earlier in the document does not hide an unsupported one after it, and the
    /// line reported is the later tag's.
    #[test]
    fn the_first_unsupported_tag_after_supported_ones_is_reported() {
        unsafe { std::env::set_var("RIFT_TEST_1095_A", "a") };
        let content = "{\"a\": \"<%= process.env.RIFT_TEST_1095_A %>\",\n\"b\": \"<%= other %>\"}";
        let result = preprocess_ejs(content, &PathBuf::from("c.json"), EjsFileAccess::Allowed);
        unsafe { std::env::remove_var("RIFT_TEST_1095_A") };
        let err = result
            .expect_err("the second tag is unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= other %>` at c.json:2,"),
            "{err}"
        );
    }

    /// A long tag is truncated in the message rather than pasting a whole document into it.
    #[test]
    fn a_long_unsupported_tag_is_truncated_in_the_message() {
        let body = "x".repeat(200);
        let content = format!(r#"{{"a": "<% {body} %>"}}"#);
        let err = preprocess_ejs(&content, &PathBuf::from("c.json"), EjsFileAccess::Allowed)
            .expect_err("unsupported")
            .to_string();
        let expected_tag = format!("`<% {}…`", "x".repeat(76));
        assert!(
            err.starts_with(&format!("unsupported EJS tag {expected_tag} at c.json:1,")),
            "{err}"
        );
    }

    /// An env var's value is content: a `<%` in it is neither refused nor substituted again.
    #[test]
    fn a_tag_inside_an_env_value_is_left_as_content() {
        unsafe { std::env::set_var("RIFT_TEST_1095_ENV", "<%= not a tag %>") };
        let content = r#"{"b": "<%= process.env.RIFT_TEST_1095_ENV %>"}"#;
        let result = preprocess_ejs(content, &PathBuf::from("c.json"), EjsFileAccess::Allowed);
        unsafe { std::env::remove_var("RIFT_TEST_1095_ENV") };
        assert_eq!(result.unwrap(), r#"{"b": "<%= not a tag %>"}"#);
    }

    /// A stringified file is rendered before it is escaped, as Mountebank's formatter does: its env
    /// tags are substituted, and text it inserts is not rescanned against the rest of the document.
    #[test]
    fn a_stringified_file_is_rendered_by_its_own_spans() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("f.js"),
            "var host = \"<%= process.env.RIFT_TEST_1095_HOST %>\";",
        )
        .unwrap();
        unsafe { std::env::set_var("RIFT_TEST_1095_HOST", "h") };
        unsafe { std::env::set_var("RIFT_TEST_1095_PORT", "8080") };
        let content =
            r#"{"a": "<%- stringify('f.js') %>", "b": "<%= process.env.RIFT_TEST_1095_PORT %>"}"#;
        let result = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed);
        unsafe { std::env::remove_var("RIFT_TEST_1095_HOST") };
        unsafe { std::env::remove_var("RIFT_TEST_1095_PORT") };
        assert_eq!(
            result.unwrap(),
            r#"{"a": "var host = \"h\";", "b": "8080"}"#
        );
    }

    /// Before the one-pass render, an unclosed `<%=` inside a stringified file ran on to the `%>` of
    /// the next real tag, and that tag was left unsubstituted with no error. It is refused instead,
    /// at its line in the stringified file.
    #[test]
    fn an_unclosed_tag_in_a_stringified_file_is_refused_where_it_is() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.js"), "// ok\nvar open = \"<%=\";").unwrap();
        let content = "{\n\"a\": \"<%- stringify('f.js') %>\", \"b\": \"<%= process.env.HOME %>\"}";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("the stringified file holds an unclosed tag")
            .to_string();
        assert!(
            err.starts_with(
                "unsupported EJS tag `<%=\";` (no closing `%>`) at f.js:2, stringified at "
            ) && err.contains("c.json:2,"),
            "{err}"
        );
    }

    #[test]
    fn a_stringify_inside_a_stringified_file_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("inner.txt"), "x").unwrap();
        std::fs::write(
            dir.path().join("outer.txt"),
            "<%- stringify('inner.txt') %>",
        )
        .unwrap();
        let content = r#"{"a": "<%- stringify('outer.txt') %>"}"#;
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("nested stringify is not evaluated")
            .to_string();
        assert!(
            err.starts_with(
                "unsupported EJS tag `<%- stringify('inner.txt') %>` at outer.txt:1, stringified at "
            ) && err.contains("(a stringify inside a stringified file is not evaluated)"),
            "{err}"
        );
    }

    /// Included text is templated like the document itself, so an unsupported tag there is refused
    /// too, at its line in the included file and the line that included it.
    #[test]
    fn an_unsupported_tag_in_an_included_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("part.json"), "{\n\"port\": <%= port %>}").unwrap();
        let content = "{\"imposters\": [\n<% include 'part.json' %>]}";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("the included tag is unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= port %>` at part.json:2, included at ")
                && err.contains("c.json:2,"),
            "{err}"
        );
    }

    /// The same tag text in the included file and later in the document: the failure is the
    /// included one, and it is not blamed on the document's line.
    #[test]
    fn a_tag_repeated_in_an_include_and_the_document_is_attributed_to_the_include() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("inc.txt"), "<%= other %>").unwrap();
        let content = "{\n  \"inc\": \"<% include 'inc.txt' %>\",\n  \"b\": \"<%= other %>\"\n}";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("both occurrences are unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= other %>` at inc.txt:1, included at ")
                && err.contains("c.json:2,"),
            "{err}"
        );
    }

    /// Lines after an include are counted in the original document, whatever the include expanded
    /// to — here a multi-line file, then an empty one.
    #[test]
    fn a_tag_after_includes_is_reported_at_its_original_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("three.txt"), "1\n2\n3").unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let content = "{\n\"x\": \"<% include 'three.txt' %>\",\n\"y\": \"<% include 'empty.txt' %>\",\n\"z\": \"<%= bad %>\"\n}";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= bad %>` at ") && err.contains("c.json:4,"),
            "{err}"
        );
    }

    /// An include longer than its tag, with a tag right at the end of the included text: the tag is
    /// the document's, on the include's line.
    #[test]
    fn a_tag_right_after_a_longer_include_is_the_documents() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.json"), "{\n  \"a\": 1,\n  \"b\": 2\n}").unwrap();
        let content = "x\n<% include 'big.json' %><%= bad %>\n";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= bad %>` at ") && err.contains("c.json:2,"),
            "{err}"
        );
    }

    #[test]
    fn a_nested_include_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("common.json"), "{}").unwrap();
        std::fs::write(dir.path().join("part.json"), "<% include 'common.json' %>").unwrap();
        let content = "[<% include 'part.json' %>,\n<% include 'common.json' %>]";
        let err = preprocess_ejs(content, &dir.path().join("c.json"), EjsFileAccess::Allowed)
            .expect_err("a nested include is not evaluated")
            .to_string();
        assert!(
            err.starts_with(
                "unsupported EJS tag `<% include 'common.json' %>` at part.json:1, included at "
            ) && err.contains(
                "c.json:1 (an include inside an included or stringified file is not evaluated),"
            ),
            "{err}"
        );
    }

    #[test]
    fn line_counting_holds_with_crlf_line_endings() {
        let content = "{\r\n  \"a\": 1,\r\n  \"b\": \"<%= x %>\"\r\n}";
        let err = preprocess_ejs(content, &PathBuf::from("c.json"), EjsFileAccess::Allowed)
            .expect_err("unsupported")
            .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= x %>` at c.json:3,"),
            "{err}"
        );
    }

    #[test]
    fn a_bare_lt_percent_gt_is_unterminated() {
        let err = preprocess_ejs(
            r#"{"a": "<%>"}"#,
            &PathBuf::from("c.json"),
            EjsFileAccess::Allowed,
        )
        .expect_err("unsupported")
        .to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%>\"}` (no closing `%>`) at c.json:1,"),
            "{err}"
        );
    }

    #[test]
    fn an_unsupported_tag_right_after_a_supported_one_is_found() {
        unsafe { std::env::set_var("RIFT_TEST_1095_ADJ", "v") };
        let content = r#"{"a": "<%= process.env.RIFT_TEST_1095_ADJ %><%= bad %>"}"#;
        let result = preprocess_ejs(content, &PathBuf::from("c.json"), EjsFileAccess::Allowed);
        unsafe { std::env::remove_var("RIFT_TEST_1095_ADJ") };
        let err = result.expect_err("unsupported").to_string();
        assert!(
            err.starts_with("unsupported EJS tag `<%= bad %>` at c.json:1,"),
            "{err}"
        );
    }

    /// A fetched document has no `--no-parse`, so its refusal points at the source instead.
    #[test]
    fn a_remote_document_refusal_does_not_suggest_no_parse() {
        let err = preprocess_ejs(
            r#"{"a": "<% x %>"}"#,
            Path::new("https://host/imposters.json"),
            EjsFileAccess::Denied,
        )
        .expect_err("unsupported")
        .to_string();
        assert_eq!(
            err,
            "unsupported EJS tag `<% x %>` at https://host/imposters.json:1, so the document was \
             not loaded. Only `<%= process.env.VAR %>` and `<%= process.env.VAR || 'default' %>` \
             are evaluated in a fetched document, and it is always preprocessed: remove the tag \
             at the source."
        );
    }

    /// `--no-parse` is the escape hatch for a literal tag, through the real loader.
    #[test]
    fn no_parse_loads_a_literal_tag_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "imposters.json",
            r#"{"imposters": [{"port": 4545, "protocol": "http", "stubs": [{"responses": [{"is": {"body": "<% not a template %>"}}]}]}]}"#,
        );
        let untouched = load_configs(&ConfigSource::File {
            path: path.clone(),
            no_parse: true,
        })
        .expect("--no-parse skips preprocessing");
        let body =
            serde_json::to_value(&untouched[0]).unwrap()["stubs"][0]["responses"][0]["is"]["body"]
                .clone();
        assert_eq!(body, serde_json::json!("<% not a template %>"));

        let err = load_configs(&ConfigSource::File {
            path,
            no_parse: false,
        })
        .expect_err("preprocessing refuses the literal tag");
        assert!(
            format!("{err:#}").contains("`<% not a template %>`"),
            "{err:#}"
        );
    }

    /// Issue #1158: an interrupted write leaves `{port}.json.tmp`, possibly partial. A reload must
    /// neither read it nor remove it — removing it could fail a live writer's rename.
    #[test]
    fn a_reload_ignores_and_keeps_an_interrupted_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("4545.json"),
            r#"{"port":4545,"protocol":"http","stubs":[]}"#,
        )
        .expect("write");
        std::fs::write(dir.path().join("4545.json.tmp"), "{ truncated").expect("write");
        let loaded = load_dir(dir.path()).expect("the partial temp file is not read");
        assert_eq!(loaded.len(), 1);
        assert!(dir.path().join("4545.json.tmp").exists());
    }

    /// Issue #1152: `returnCaKey` asks for the generated key in the start response, and a config
    /// file has none — the key used to be minted and dropped. Refused, naming the key; `false`
    /// still loads.
    #[test]
    fn a_config_file_intercept_block_cannot_ask_for_the_ca_key() {
        let doc = |value: bool| {
            format!(r#"{{"imposters": [], "intercept": {{"returnCaKey": {value}}}}}"#)
        };
        let err = parse_document(&doc(true), &ScriptBaseDir::Unconfigured)
            .expect_err("returnCaKey: true is refused");
        assert!(format!("{err:#}").contains("returnCaKey"), "{err:#}");
        let loaded =
            parse_document(&doc(false), &ScriptBaseDir::Unconfigured).expect("false loads");
        assert!(loaded.intercept.is_some());
    }
}
