//! Loading imposter configs from the CLI-provided source (`--configfile` / `--datadir`), shared
//! by startup and the `POST /admin/reload` hot-reload endpoint (issue #197). Parsing is pure (no
//! running state is touched), so a parse error is returned rather than applied.

use crate::imposter::ImposterConfig;
use std::path::{Path, PathBuf};
use tracing::warn;

/// Where the running imposters were loaded from, retained so reload can re-read the same source.
#[derive(Debug, Clone)]
pub enum ConfigSource {
    /// A single `--configfile` (Mountebank JSON/YAML, with optional EJS preprocessing).
    File { path: PathBuf, no_parse: bool },
    /// A `--datadir` of one-imposter-per-`.json` files.
    Dir(PathBuf),
}

/// Parse the source into imposter configs without creating any imposters. A parse error is
/// returned so the caller (startup or hot-reload) decides whether to apply the result.
pub fn load_configs(source: &ConfigSource) -> anyhow::Result<Vec<ImposterConfig>> {
    match source {
        ConfigSource::File { path, no_parse } => load_file(path, *no_parse),
        ConfigSource::Dir(dir) => load_dir(dir),
    }
}

fn load_file(path: &Path, no_parse: bool) -> anyhow::Result<Vec<ImposterConfig>> {
    let raw = std::fs::read_to_string(path)?;
    let content = if no_parse {
        raw
    } else {
        preprocess_ejs(&raw, path)?
    };

    let trimmed = content.trim_start();
    let configs: Vec<ImposterConfig> = if trimmed.starts_with('{') {
        // Single imposter, or a `{ "imposters": [...] }` wrapper (Mountebank format).
        let value: serde_json::Value = serde_json::from_str(&content)?;
        match value.get("imposters") {
            Some(imposters) => serde_json::from_value(imposters.clone())?,
            None => vec![serde_json::from_value(value)?],
        }
    } else if trimmed.starts_with('[') {
        serde_json::from_str(&content)?
    } else {
        serde_yaml::from_str(&content)?
    };
    Ok(configs)
}

fn load_dir(dir: &Path) -> anyhow::Result<Vec<ImposterConfig>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut configs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let content = std::fs::read_to_string(&path)?;
            configs.push(serde_json::from_str::<ImposterConfig>(&content)?);
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
    let include_re = regex::Regex::new(r#"<%\s*include\s+['"]?([^'">\s]+)['"]?\s*%>"#).unwrap();
    let mut result = String::new();
    let mut last = 0;
    for cap in include_re.captures_iter(content) {
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

    // Process expression tags: `<%= expr %>`
    let expr_re = regex::Regex::new(r"<%=\s*(.*?)\s*%>").unwrap();
    let env_var_re = regex::Regex::new(
        r#"^process\.env\.([A-Za-z_][A-Za-z0-9_]*)(?:\s*\|\|\s*['"]([^'"]*)['"]\s*)?$"#,
    )
    .unwrap();

    let mut result = String::new();
    let mut last = 0;
    for cap in expr_re.captures_iter(&content) {
        let full = cap.get(0).unwrap();
        let expr = cap.get(1).unwrap().as_str().trim();
        result.push_str(&content[last..full.start()]);

        if let Some(env_cap) = env_var_re.captures(expr) {
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

    // Strip remaining `<% ... %>` control blocks (non-expression tags); (?s) enables dotall
    let stmt_re = regex::Regex::new(r"(?s)<%[^=].*?%>").unwrap();
    if stmt_re.is_match(&content) {
        warn!("EJS statement blocks (<% ... %>) are not supported and will be removed");
    }
    Ok(stmt_re.replace_all(&content, "").to_string())
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

    #[test]
    fn parse_error_is_returned_not_panicked() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(dir.path(), "bad.json", "{ not valid json");
        assert!(load_configs(&ConfigSource::File {
            path: bad,
            no_parse: false
        })
        .is_err());
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
        std::env::set_var("RIFT_TEST_HOST", "myhost");
        let content = r#"{"body": "<%= process.env.RIFT_TEST_HOST %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"body": "myhost"}"#);
        std::env::remove_var("RIFT_TEST_HOST");
    }

    #[test]
    fn test_ejs_env_var_with_default() {
        std::env::remove_var("RIFT_TEST_UNSET_VAR");
        let content = r#"{"port": "<%= process.env.RIFT_TEST_UNSET_VAR || '4545' %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "4545"}"#);
    }

    #[test]
    fn test_ejs_env_var_present_overrides_default() {
        std::env::set_var("RIFT_TEST_PORT", "8080");
        let content = r#"{"port": "<%= process.env.RIFT_TEST_PORT || '4545' %>"}"#;
        let path = PathBuf::from("config.json");
        let result = preprocess_ejs(content, &path).unwrap();
        assert_eq!(result, r#"{"port": "8080"}"#);
        std::env::remove_var("RIFT_TEST_PORT");
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
}
