//! Loading imposter configs from the CLI-provided source (`--configfile` / `--datadir`), shared
//! by startup and the `POST /admin/reload` hot-reload endpoint (issue #197). Parsing is pure (no
//! running state is touched), so a parse error is returned rather than applied.

use crate::imposter::{ImposterConfig, ScriptBaseDir, resolve_scripts};
use crate::intercept_control::InterceptStartOptions;
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tracing::warn;

// Fixed EJS tag patterns (issue #560): compile once at first use rather than on every
// `preprocess_ejs` call — that runs per config file at startup, on every `POST /admin/reload`, and
// from the script CLI. All are compile-time-constant patterns, so a compile failure is a
// programming error caught immediately by tests, not a data-dependent runtime error.

/// `<% include 'path' %>` — quoted or bare path.
static EJS_INCLUDE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<%\s*include\s+['"]?([^'">\s]+)['"]?\s*%>"#)
        .expect("EJS include pattern is a valid constant regex")
});

/// `<%- stringify('relative/path') %>` (issue #355 Item 7).
static EJS_STRINGIFY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<%-\s*stringify\(\s*['"]([^'"]+)['"]\s*\)\s*%>"#)
        .expect("EJS stringify pattern is a valid constant regex")
});

/// `<%= expr %>` expression tag.
static EJS_EXPR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<%=\s*(.*?)\s*%>").expect("EJS expression pattern is a valid constant regex")
});

/// The only supported expression body: `process.env.VAR` with an optional `|| 'default'`.
static EJS_ENV_VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^process\.env\.([A-Za-z_][A-Za-z0-9_]*)(?:\s*\|\|\s*['"]([^'"]*)['"]\s*)?$"#)
        .expect("EJS env-var pattern is a valid constant regex")
});

/// Remaining `<% ... %>` control blocks (non-expression tags); `(?s)` enables dotall.
static EJS_STMT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<%[^=].*?%>").expect("EJS statement pattern is a valid constant regex")
});

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
        ConfigSource::Dir(dir) => load_dir(dir).map(|imposters| LoadedConfig {
            imposters,
            intercept: None,
        }),
    }
}

fn load_file(path: &Path, no_parse: bool) -> anyhow::Result<LoadedConfig> {
    let raw = std::fs::read_to_string(path)?;
    let content = if no_parse {
        raw
    } else {
        preprocess_ejs(&raw, path)?
    };

    let trimmed = content.trim_start();
    let mut intercept = None;
    let mut configs: Vec<ImposterConfig> = if trimmed.starts_with('{') {
        // Single imposter, or a `{ "imposters": [...] }` wrapper (Mountebank format).
        let value: serde_json::Value = serde_json::from_str(&content)?;
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
                serde_json::from_value(imposters.clone())?
            }
            // A single-imposter document has no `imposters` key, so it has no wrapper to carry a
            // listener declaration — and `ImposterConfig` ignores unknown fields, so an `intercept`
            // key here would be dropped without a word: no listener, no rule, no diagnostic, and a
            // green boot (issue #655). Refuse instead; the remedy is one line of JSON.
            None if value.get("intercept").is_some() => anyhow::bail!(
                "an `intercept` block is only read from the `{{\"imposters\": [...], \"intercept\": {{...}}}}` \
                 wrapper form, but this document has no `imposters` key, so the block would be ignored. \
                 Wrap the imposter in `\"imposters\": [ ... ]` (use `[]` if the file declares none)."
            ),
            None => vec![serde_json::from_value(value)?],
        }
    } else if trimmed.starts_with('[') {
        serde_json::from_str(&content)?
    } else {
        serde_yaml::from_str(&content)?
    };

    // Resolve `_rift.script` `file:`/`ref:` sources (issue #356) relative to the config file's
    // own directory, before the configs are handed to the caller — so a parse error and a
    // resolve error are both surfaced up front, and hot-reload (which re-runs this loader)
    // automatically picks up edits to referenced script files.
    let base = ScriptBaseDir::ConfigRelative(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    );
    for config in &mut configs {
        resolve_scripts(config, &base)?;
    }
    Ok(LoadedConfig {
        imposters: configs,
        intercept,
    })
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
            let content = std::fs::read_to_string(&path)?;
            let mut config: ImposterConfig = serde_json::from_str(&content)?;
            resolve_scripts(&mut config, &base)?;
            configs.push(config);
        }
    }
    Ok(configs)
}

/// Pre-process EJS tokens in a config file before JSON/YAML parsing.
///
/// Handles the patterns emitted by Mountebank and compatible tooling:
/// - `<% include 'path' %>` — inline the referenced file (relative to the config file)
/// - `<%= process.env.VAR %>` — substitute with the env var value (empty string if unset)
/// - `<%= process.env.VAR || 'default' %>` — substitute with env var or the literal default
///
/// Any other `<%= expr %>` token is replaced with an empty string and logged as a warning.
/// `<% expr %>` (without `=`) statements (e.g., `<% for (...) %>`) are removed and logged.
fn preprocess_ejs(content: &str, config_path: &Path) -> anyhow::Result<String> {
    if !content.contains("<%") {
        return Ok(content.to_string());
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

    // Process include directives first:
    // `<% include 'path' %>`, `<% include "path" %>`, or `<% include path %>`
    let mut result = String::new();
    let mut last = 0;
    for cap in EJS_INCLUDE_RE.captures_iter(content) {
        let full = cap.get(0).unwrap();
        let include_path = cap.get(1).unwrap().as_str();
        result.push_str(&content[last..full.start()]);
        let abs_path = config_dir.join(include_path);
        match std::fs::read_to_string(&abs_path) {
            Ok(included) => result.push_str(&included),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "EJS include file '{}' not found ({}): {}",
                    include_path,
                    abs_path.display(),
                    e
                ));
            }
        }
        last = full.end();
    }
    result.push_str(&content[last..]);
    let content = result;

    // Process `<%- stringify('relative/path') %>` (issue #355 Item 7): inline the referenced
    // file's contents as a JSON-string-safe body. Must run BEFORE the final `<% ... %>` strip
    // below — that catch-all matches `<%[^=].*?%>`, which would otherwise eat `<%-` tokens too.
    // `<%-` is EJS's "unescaped output" tag; here the template already supplies the surrounding
    // quotes (e.g. `"inject": "<%- stringify('inject.js') %>"`), so only the escaped INNER
    // content is substituted — `serde_json::to_string` then stripping its own wrapping quotes —
    // keeping the surrounding JSON valid.
    let mut result = String::new();
    let mut last = 0;
    for cap in EJS_STRINGIFY_RE.captures_iter(&content) {
        let full = cap.get(0).unwrap();
        let rel_path = cap.get(1).unwrap().as_str();
        result.push_str(&content[last..full.start()]);
        let abs_path = config_dir.join(rel_path);
        let file_contents = std::fs::read_to_string(&abs_path).map_err(|e| {
            anyhow::anyhow!(
                "EJS stringify file '{}' not found ({}): {}",
                rel_path,
                abs_path.display(),
                e
            )
        })?;
        let json_quoted = serde_json::to_string(&file_contents).map_err(|e| {
            anyhow::anyhow!("failed to JSON-encode stringify file '{rel_path}': {e}")
        })?;
        // Strip the wrapping quotes serde_json added; the template's own quotes surround the tag.
        let inner = &json_quoted[1..json_quoted.len() - 1];
        result.push_str(inner);
        last = full.end();
    }
    result.push_str(&content[last..]);
    let content = result;

    // Process expression tags: `<%= expr %>`
    let mut result = String::new();
    let mut last = 0;
    for cap in EJS_EXPR_RE.captures_iter(&content) {
        let full = cap.get(0).unwrap();
        let expr = cap.get(1).unwrap().as_str().trim();
        result.push_str(&content[last..full.start()]);

        if let Some(env_cap) = EJS_ENV_VAR_RE.captures(expr) {
            let var_name = env_cap.get(1).unwrap().as_str();
            let default_val = env_cap.get(2).map(|m| m.as_str()).unwrap_or("");
            let value = std::env::var(var_name).unwrap_or_else(|_| default_val.to_string());
            result.push_str(&value);
        } else {
            warn!(
                "EJS expression '{}' is not supported; substituting empty string",
                expr
            );
        }
        last = full.end();
    }
    result.push_str(&content[last..]);
    let content = result;

    // Strip remaining `<% ... %>` control blocks (non-expression tags).
    if EJS_STMT_RE.is_match(&content) {
        warn!("EJS statement blocks (<% ... %>) are not supported and will be removed");
    }
    Ok(EJS_STMT_RE.replace_all(&content, "").to_string())
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
        write(dir.path(), "a.json", r#"{"port":8100,"protocol":"http"}"#);
        write(dir.path(), "b.json", r#"{"port":8101,"protocol":"http"}"#);
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
            let msg = err.to_string();
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
            "rules": [{"host":"cdn.example.com","action":{"forward":{"port":4545}}}]
        }
    }"#;

    /// AC2: the block is read from the same wrapper object that already carries `imposters`.
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

    /// A `--datadir` has no wrapper object to carry a block.
    #[test]
    fn load_configs_full_from_dir_has_no_intercept_block() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.json", r#"{"port":8100,"protocol":"http"}"#);
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
        assert_eq!(preprocess_ejs(content, &path).unwrap(), content);
    }

    #[test]
    fn test_ejs_env_var_substitution() {
        unsafe { std::env::set_var("RIFT_TEST_HOST", "myhost") };
        let content = r#"{"body": "<%= process.env.RIFT_TEST_HOST %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"body": "myhost"}"#);
        unsafe { std::env::remove_var("RIFT_TEST_HOST") };
    }

    #[test]
    fn test_ejs_env_var_with_default() {
        unsafe { std::env::remove_var("RIFT_TEST_UNSET_VAR") };
        let content = r#"{"port": "<%= process.env.RIFT_TEST_UNSET_VAR || '4545' %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "4545"}"#);
    }

    #[test]
    fn test_ejs_env_var_present_overrides_default() {
        unsafe { std::env::set_var("RIFT_TEST_PORT", "8080") };
        let content = r#"{"port": "<%= process.env.RIFT_TEST_PORT || '4545' %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "8080"}"#);
        unsafe { std::env::remove_var("RIFT_TEST_PORT") };
    }

    #[test]
    fn test_ejs_include_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("partial.json"), r#"{"key": "value"}"#).unwrap();
        let content = r#"<% include 'partial.json' %>"#.to_string();
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(&content, &config_path).unwrap();
        assert_eq!(result, r#"{"key": "value"}"#);
    }

    #[test]
    fn test_ejs_include_unquoted_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("partial.json"), r#"[1,2,3]"#).unwrap();
        let content = r#"<% include partial.json %>"#;
        let config_path = dir.path().join("config.ejs");
        let result = preprocess_ejs(content, &config_path).unwrap();
        assert_eq!(result, "[1,2,3]");
    }

    #[test]
    fn test_ejs_missing_include_is_fatal_error() {
        let content = r#"<% include 'nonexistent.json' %>"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path);
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
        let processed = preprocess_ejs(content, &config_path).unwrap();

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
        let result = preprocess_ejs(content, &path);
        assert!(result.is_err(), "missing stringify file should return Err");
        assert!(
            result.unwrap_err().to_string().contains("nope-355.js"),
            "error message should name the missing file"
        );
    }

    /// Pins the ordering invariant documented at the `<%- stringify %>` step: it must run BEFORE
    /// the `EJS_STMT_RE` catch-all, which also matches `<%- ... %>` and would otherwise silently
    /// eat the tag — wrong output, no error. Neither tag type alone catches a reordering.
    #[test]
    fn ejs_stringify_survives_when_statement_blocks_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("inject.js"), "hi").unwrap();
        let content = r#"{"a": "<%- stringify('inject.js') %>"}<% if (x) { %><% } %>"#;
        let config_path = dir.path().join("config.ejs");
        assert_eq!(
            preprocess_ejs(content, &config_path).unwrap(),
            r#"{"a": "hi"}"#
        );
    }

    #[test]
    fn ejs_statement_blocks_are_stripped() {
        let content = r#"{"a": 1<% for (var i=0;i<3;i++) { %><% } %>}"#;
        let path = PathBuf::from("config.json");
        assert_eq!(preprocess_ejs(content, &path).unwrap(), r#"{"a": 1}"#);
    }

    #[test]
    fn ejs_statement_strip_spans_newlines() {
        let content = "{\"a\": 1<% if (x) {\n  y();\n} %>}";
        let path = PathBuf::from("config.json");
        assert_eq!(preprocess_ejs(content, &path).unwrap(), r#"{"a": 1}"#);
    }

    #[test]
    fn ejs_statics_match_their_tags() {
        use super::{EJS_ENV_VAR_RE, EJS_EXPR_RE, EJS_INCLUDE_RE, EJS_STMT_RE, EJS_STRINGIFY_RE};

        assert_eq!(
            EJS_INCLUDE_RE
                .captures(r#"<% include 'a/b.json' %>"#)
                .unwrap()[1]
                .to_string(),
            "a/b.json"
        );
        assert_eq!(
            EJS_INCLUDE_RE.captures("<% include bare.json %>").unwrap()[1].to_string(),
            "bare.json"
        );

        assert_eq!(
            EJS_STRINGIFY_RE
                .captures(r#"<%- stringify('inject.js') %>"#)
                .unwrap()[1]
                .to_string(),
            "inject.js"
        );

        assert_eq!(
            EJS_EXPR_RE.captures("<%= process.env.HOST %>").unwrap()[1].to_string(),
            "process.env.HOST"
        );

        let env_cap = EJS_ENV_VAR_RE
            .captures("process.env.PORT || '4545'")
            .unwrap();
        assert_eq!(env_cap[1].to_string(), "PORT");
        assert_eq!(env_cap[2].to_string(), "4545");
        assert!(
            EJS_ENV_VAR_RE
                .captures("process.env.HOST")
                .unwrap()
                .get(2)
                .is_none()
        );
        assert!(EJS_ENV_VAR_RE.captures("someOtherExpr()").is_none());

        // (?s) dotall: a statement block spanning newlines is one match.
        assert!(EJS_STMT_RE.is_match("<% if (x) {\n y();\n} %>"));
        // `<%=` is an expression tag, not a statement — the catch-all must not eat it.
        assert!(!EJS_STMT_RE.is_match("<%= process.env.HOST %>"));
    }
}
