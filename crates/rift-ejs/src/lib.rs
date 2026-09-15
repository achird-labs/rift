//! The EJS template subset Rift's config loader evaluates, shared by the engine and `rift-lint`
//! (issue #1108) so both read a templated config the same way.
//!
//! The four supported tags and every refusal are documented on [`render`]. This crate has no rift
//! dependencies, which is what lets `rift-lint` use it: `rift-http-proxy` dev-depends on `rift-lint`.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

// Fixed EJS tag patterns (issue #560): compile once at first use rather than on every
// `render` call — that runs per config file at startup, on every `POST /admin/reload`, and
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

/// Whether EJS tags that read the local filesystem are honoured.
///
/// `--configfile` documents are authored by someone who already has filesystem access, so
/// `<% include %>` / `<%- stringify %>` are resolved verbatim — the same rationale that makes
/// rift-mock-core's `ScriptBaseDir::ConfigRelative` unrestricted. A document fetched over the network (U-12's
/// `https:` source) has no such author, so those two tags are refused rather than resolved:
/// honouring them would let whoever serves the document read arbitrary local files. `<%=
/// process.env.X %>` still substitutes in both — env is deployment config the operator chose to
/// expose to their own process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess {
    /// Local document: `include` and `stringify` resolve against the document's directory.
    Allowed,
    /// Remote document: `include` and `stringify` are a load error naming the tag.
    Denied,
}

/// A document with its tags rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Each `<%= process.env.VAR %>` with no default whose variable was unset, so it rendered empty.
    pub unset_env: Vec<UnsetEnv>,
}

/// An environment variable a tag read while it was unset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsetEnv {
    pub name: String,
    /// Where the tag is, as `at <file>:<line>` (with the including file for an included one).
    pub place: String,
}

/// Why a document could not be rendered. Each message names the tag or file and where it is.
#[derive(Debug, thiserror::Error)]
pub enum EjsError {
    /// A tag the loader does not evaluate (issue #1095).
    #[error("{0}")]
    UnsupportedTag(String),
    /// An `include` or `stringify` in a document fetched from somewhere other than local disk.
    #[error("{0}")]
    LocalFileRefused(String),
    #[error("EJS include file '{file}' not found ({}): {io}", path.display())]
    Include {
        file: String,
        path: PathBuf,
        io: std::io::Error,
    },
    #[error("EJS stringify file '{file}' not found ({}): {io}", path.display())]
    Stringify {
        file: String,
        path: PathBuf,
        io: std::io::Error,
    },
    #[error("failed to JSON-encode stringify file '{file}': {json}")]
    Encode {
        file: String,
        json: serde_json::Error,
    },
}

/// True when `content` holds anything that looks like a tag, so [`render`] would change or refuse it.
#[must_use]
pub fn has_tags(content: &str) -> bool {
    content.contains("<%")
}

/// Pre-process EJS tokens in a config file before JSON/YAML parsing.
///
/// Handles the patterns emitted by Mountebank and compatible tooling:
/// - `<% include 'path' %>` — inline the referenced file (relative to the config file)
/// - `<%- stringify('path') %>` — inline a file's rendered contents, escaped for a JSON string
/// - `<%= process.env.VAR %>` — substitute with the env var value (empty string if unset)
/// - `<%= process.env.VAR || 'default' %>` — substitute with env var or the literal default
///
/// Any other tag — another `<%= expr %>`, a `<% statement %>`, `<%- expr %>`, `<%# comment %>`, a
/// `<%` with no closing `%>` — fails the load, naming the tag and where it is (issue #1095).
/// Stripping it would load a config that silently differs from the file.
///
/// Includes are expanded first, so an included file is templated like the document. Every other
/// tag is then substituted by its own span in one pass, so text a substitution inserts — an env
/// var's value, a stringified file — is never scanned again as if it were template.
///
/// A `<%= process.env.VAR %>` whose variable is unset renders empty, as Mountebank's does; each one
/// is listed in [`Rendered::unset_env`] so a caller can say so. One with a `|| 'default'` is not.
///
/// # Errors
///
/// [`EjsError`] names the first tag that cannot be rendered and where it is.
pub fn render(
    content: &str,
    config_path: &Path,
    file_access: FileAccess,
) -> Result<Rendered, EjsError> {
    if !has_tags(content) {
        return Ok(Rendered {
            text: content.to_string(),
            unset_env: Vec::new(),
        });
    }

    // Fail closed before any resolution: a remote document that names a local file is refused
    // outright, naming the tag.
    if file_access == FileAccess::Denied {
        for (re, tag) in [
            (&*EJS_INCLUDE_RE, "<% include ... %>"),
            (&*EJS_STRINGIFY_RE, "<%- stringify(...) %>"),
        ] {
            if let Some(cap) = re.captures(content) {
                return Err(EjsError::LocalFileRefused(format!(
                    "`{tag}` reads a local file and is not honoured in a document fetched from \
                     {} — it names '{}'. Only local `--configfile` documents may include local \
                     files; use `--configfile` if the template must, or inline the content at \
                     the source.",
                    config_path.display(),
                    &cap[1],
                )));
            }
        }
    }

    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let expanded = expand_includes(content, config_dir)?;
    let locate = |offset: usize| expanded.locate(offset, content, config_path);
    let mut unset_env = Vec::new();
    let text = render_tags(
        &expanded.text,
        TagScope::Document,
        config_dir,
        file_access,
        &locate,
        &mut unset_env,
    )?;
    Ok(Rendered { text, unset_env })
}

/// A document with its `<% include %>` tags replaced by the files they name.
#[derive(Debug)]
struct ExpandedDocument {
    text: String,
    includes: Vec<IncludedSpan>,
}

/// Where one included file landed in [`ExpandedDocument::text`].
#[derive(Debug)]
struct IncludedSpan {
    /// The included file's bytes within the expanded text.
    range: std::ops::Range<usize>,
    /// The include tag's position and length in the original document.
    tag_offset: usize,
    tag_len: usize,
    /// The path as the tag wrote it.
    file: String,
}

impl ExpandedDocument {
    /// Describe `offset` in the expanded text as a place a reader can open: a line of the original
    /// document, or a line of the included file together with the line that included it.
    fn locate(&self, offset: usize, original: &str, config_path: &Path) -> String {
        let mut inserted = 0;
        let mut removed = 0;
        for span in &self.includes {
            if span.range.contains(&offset) {
                let local = &self.text[span.range.clone()];
                return format!(
                    "at {}:{}, included at {}:{}",
                    span.file,
                    line_of(local, offset - span.range.start),
                    config_path.display(),
                    line_of(original, span.tag_offset)
                );
            }
            if span.range.end <= offset {
                inserted += span.range.len();
                removed += span.tag_len;
            }
        }
        let original_offset = offset + removed - inserted;
        format!(
            "at {}:{}",
            config_path.display(),
            line_of(original, original_offset)
        )
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

fn expand_includes(content: &str, config_dir: &Path) -> Result<ExpandedDocument, EjsError> {
    let mut text = String::with_capacity(content.len());
    let mut includes = Vec::new();
    let mut last = 0;
    for cap in EJS_INCLUDE_RE.captures_iter(content) {
        let (Some(full), Some(include_path)) = (cap.get(0), cap.get(1)) else {
            continue;
        };
        text.push_str(&content[last..full.start()]);
        let abs_path = config_dir.join(include_path.as_str());
        let included = std::fs::read_to_string(&abs_path).map_err(|io| EjsError::Include {
            file: include_path.as_str().to_string(),
            path: abs_path.clone(),
            io,
        })?;
        let start = text.len();
        text.push_str(&included);
        includes.push(IncludedSpan {
            range: start..text.len(),
            tag_offset: full.start(),
            tag_len: full.len(),
            file: include_path.as_str().to_string(),
        });
        last = full.end();
    }
    text.push_str(&content[last..]);
    Ok(ExpandedDocument { text, includes })
}

/// What template text is being rendered, which decides the tags it may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagScope {
    /// The document, includes already expanded.
    Document,
    /// A file a `<%- stringify %>` tag inlines. It is rendered like the document before it is
    /// escaped (Mountebank's formatter does the same), but it may not include or stringify again.
    Stringified,
}

/// Substitute every tag in `text` by its own span, refusing the first one that is not evaluated.
/// `locate` turns an offset in `text` into the place named in an error.
fn render_tags(
    text: &str,
    scope: TagScope,
    config_dir: &Path,
    file_access: FileAccess,
    locate: &dyn Fn(usize) -> String,
    unset_env: &mut Vec<UnsetEnv>,
) -> Result<String, EjsError> {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(found) = text[from..].find("<%") {
        let offset = from + found;
        out.push_str(&text[from..offset]);
        let Some(close) = text[offset + 2..].find("%>") else {
            let tag = UnsupportedTag {
                text: &text[offset..],
                terminated: false,
                note: "",
            };
            return Err(EjsError::UnsupportedTag(
                tag.message(&locate(offset), file_access),
            ));
        };
        let end = offset + 2 + close + 2;
        let tag = &text[offset..end];

        if let Some(expression) = env_expression(tag) {
            if let Some(name) = expression.unset {
                unset_env.push(UnsetEnv {
                    name: name.to_string(),
                    place: locate(offset),
                });
            }
            out.push_str(&expression.value);
        } else if let Some(rel_path) = whole_tag_capture(&EJS_STRINGIFY_RE, tag)
            && scope == TagScope::Document
        {
            out.push_str(&stringified(
                rel_path,
                config_dir,
                file_access,
                &|inner| format!("{}, stringified {}", inner, locate(offset)),
                unset_env,
            )?);
        } else {
            let note = if whole_tag_capture(&EJS_INCLUDE_RE, tag).is_some() {
                " (an include inside an included or stringified file is not evaluated)"
            } else if whole_tag_capture(&EJS_STRINGIFY_RE, tag).is_some() {
                " (a stringify inside a stringified file is not evaluated)"
            } else {
                ""
            };
            let tag = UnsupportedTag {
                text: tag,
                terminated: true,
                note,
            };
            return Err(EjsError::UnsupportedTag(
                tag.message(&locate(offset), file_access),
            ));
        }
        from = end;
    }
    out.push_str(&text[from..]);
    Ok(out)
}

/// The file `rel_path` names, rendered and escaped for use inside a JSON string (issue #355 Item
/// 7). The template supplies the surrounding quotes (`"inject": "<%- stringify('inject.js') %>"`),
/// so only the escaped inner content is returned. `outer` wraps a place in the file with where the
/// stringify tag is.
fn stringified(
    rel_path: &str,
    config_dir: &Path,
    file_access: FileAccess,
    outer: &dyn Fn(String) -> String,
    unset_env: &mut Vec<UnsetEnv>,
) -> Result<String, EjsError> {
    let abs_path = config_dir.join(rel_path);
    let contents = std::fs::read_to_string(&abs_path).map_err(|io| EjsError::Stringify {
        file: rel_path.to_string(),
        path: abs_path.clone(),
        io,
    })?;
    let locate = |offset: usize| outer(format!("at {rel_path}:{}", line_of(&contents, offset)));
    let rendered = render_tags(
        &contents,
        TagScope::Stringified,
        config_dir,
        file_access,
        &locate,
        unset_env,
    )?;
    let json_quoted = serde_json::to_string(&rendered).map_err(|json| EjsError::Encode {
        file: rel_path.to_string(),
        json,
    })?;
    // `to_string` of a `String` always yields a quoted JSON string.
    Ok(json_quoted[1..json_quoted.len() - 1].to_string())
}

/// `re`'s first capture group when `re` matches the whole of `tag`, not just part of it.
fn whole_tag_capture<'t>(re: &Regex, tag: &'t str) -> Option<&'t str> {
    let cap = re.captures(tag)?;
    let whole = cap.get(0)?;
    if whole.range() != (0..tag.len()) {
        return None;
    }
    cap.get(1).map(|m| m.as_str())
}

/// A rendered `<%= process.env.VAR %>` tag.
struct EnvExpression<'t> {
    value: String,
    /// The variable, when it is unset and the tag gives no default, so the value is empty.
    unset: Option<&'t str>,
}

/// The value of a `<%= process.env.VAR %>` / `<%= process.env.VAR || 'default' %>` tag, or `None`
/// when `tag` is not one. A variable that is set but not valid Unicode reads as unset, as it did
/// before this moved here.
fn env_expression(tag: &str) -> Option<EnvExpression<'_>> {
    let body = whole_tag_capture(&EJS_EXPR_RE, tag)?.trim();
    let env_cap = EJS_ENV_VAR_RE.captures(body)?;
    let var_name = env_cap.get(1)?.as_str();
    let default = env_cap.get(2).map(|m| m.as_str());
    Some(match (std::env::var(var_name), default) {
        (Ok(value), _) => EnvExpression { value, unset: None },
        (Err(_), Some(default)) => EnvExpression {
            value: default.to_string(),
            unset: None,
        },
        (Err(_), None) => EnvExpression {
            value: String::new(),
            unset: Some(var_name),
        },
    })
}

/// An EJS tag the preprocessor found and does not evaluate.
#[derive(Debug)]
struct UnsupportedTag<'a> {
    /// The tag as written, from `<%` through `%>` (or to the end of the text when unterminated).
    text: &'a str,
    terminated: bool,
    /// Why a tag that looks supported is not, where that applies.
    note: &'static str,
}

impl UnsupportedTag<'_> {
    const MAX_SHOWN_CHARS: usize = 80;

    /// The load error, with the tag found `place` (from [`ExpandedDocument::locate`]).
    fn message(&self, place: &str, file_access: FileAccess) -> String {
        let shown: String = if self.text.chars().count() > Self::MAX_SHOWN_CHARS {
            let head: String = self.text.chars().take(Self::MAX_SHOWN_CHARS - 1).collect();
            format!("{head}…")
        } else {
            self.text.to_string()
        };
        let unterminated = if self.terminated {
            ""
        } else {
            " (no closing `%>`)"
        };
        let note = self.note;
        match file_access {
            FileAccess::Allowed => format!(
                "unsupported EJS tag `{shown}`{unterminated} {place}{note}, so the file was not \
                 loaded. Only `<%= process.env.VAR %>`, `<%= process.env.VAR || 'default' %>`, \
                 `<% include 'file' %>` and `<%- stringify('file') %>` are evaluated. If the tag \
                 is meant literally, load the file with --no-parse (a --configfile or file: \
                 source)."
            ),
            FileAccess::Denied => format!(
                "unsupported EJS tag `{shown}`{unterminated} {place}{note}, so the document was not \
                 loaded. Only `<%= process.env.VAR %>` and `<%= process.env.VAR || 'default' %>` are \
                 evaluated in a fetched document, and it is always preprocessed: remove the tag at \
                 the source."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ejs_statics_match_their_tags() {
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
    }

    const UNSET: &str = "RIFT_EJS_TEST_1108_NEVER_SET";

    #[test]
    fn an_unset_variable_without_a_default_renders_empty_and_is_reported() {
        assert!(
            std::env::var(UNSET).is_err(),
            "{UNSET} must not be set in the test environment"
        );
        let rendered = render(
            &format!("{{\n\"body\": \"<%= process.env.{UNSET} %>\"}}"),
            Path::new("/cfg/imposters.json"),
            FileAccess::Allowed,
        )
        .expect("renders");
        assert_eq!(rendered.text, "{\n\"body\": \"\"}");
        assert_eq!(
            rendered.unset_env,
            vec![UnsetEnv {
                name: UNSET.to_string(),
                place: "at /cfg/imposters.json:2".to_string(),
            }]
        );
    }

    #[test]
    fn a_default_is_used_and_not_reported() {
        let rendered = render(
            &format!("{{\"port\": <%= process.env.{UNSET} || '4545' %>}}"),
            Path::new("imposters.json"),
            FileAccess::Allowed,
        )
        .expect("renders");
        assert_eq!(rendered.text, "{\"port\": 4545}");
        assert!(rendered.unset_env.is_empty(), "{:?}", rendered.unset_env);

        let explicit_empty = render(
            &format!("<%= process.env.{UNSET} || '' %>"),
            Path::new("imposters.json"),
            FileAccess::Allowed,
        )
        .expect("renders");
        assert_eq!(explicit_empty.text, "");
        assert!(
            explicit_empty.unset_env.is_empty(),
            "an explicit empty default is chosen"
        );
    }

    #[test]
    fn a_set_variable_is_substituted_and_not_reported() {
        let path = std::env::var("PATH").expect("PATH is set wherever tests run");
        let rendered = render(
            "<%= process.env.PATH %>",
            Path::new("x.json"),
            FileAccess::Allowed,
        )
        .expect("renders");
        assert_eq!(rendered.text, path);
        assert!(rendered.unset_env.is_empty());
    }

    #[test]
    fn an_unset_variable_inside_a_stringified_file_is_reported_with_both_places() {
        let dir = std::env::temp_dir().join(format!("rift-ejs-1108-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("inject.js"),
            format!("x\n<%= process.env.{UNSET} %>"),
        )
        .expect("write stringified file");
        let config = dir.join("imposters.json");
        let rendered = render(
            "{\"inject\": \"<%- stringify('inject.js') %>\"}",
            &config,
            FileAccess::Allowed,
        )
        .expect("renders");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(rendered.unset_env.len(), 1, "{:?}", rendered.unset_env);
        assert_eq!(
            rendered.unset_env[0].place,
            format!("at inject.js:2, stringified at {}:1", config.display())
        );
    }

    #[test]
    fn errors_keep_the_loader_messages() {
        let unsupported = render(
            "<% for (x) %>",
            Path::new("/cfg/a.json"),
            FileAccess::Allowed,
        )
        .expect_err("unsupported tag");
        assert!(matches!(unsupported, EjsError::UnsupportedTag(_)));
        assert!(
            unsupported.to_string().starts_with(
                "unsupported EJS tag `<% for (x) %>` at /cfg/a.json:1, so the file was not loaded."
            ),
            "{unsupported}"
        );

        let missing = render(
            "<% include 'nope.json' %>",
            Path::new("/cfg-1108-missing/a.json"),
            FileAccess::Allowed,
        )
        .expect_err("missing include");
        assert!(matches!(missing, EjsError::Include { .. }));
        assert!(
            missing.to_string().starts_with(
                "EJS include file 'nope.json' not found (/cfg-1108-missing/nope.json): "
            ),
            "{missing}"
        );

        // The fail-closed gate for a fetched document: it must never read a local file.
        let remote = render(
            "<% include 'secret.json' %>",
            Path::new("https://h/i.json"),
            FileAccess::Denied,
        )
        .expect_err("remote include");
        assert!(matches!(remote, EjsError::LocalFileRefused(_)));
        assert_eq!(
            remote.to_string(),
            "`<% include ... %>` reads a local file and is not honoured in a document fetched from \
             https://h/i.json — it names 'secret.json'. Only local `--configfile` documents may \
             include local files; use `--configfile` if the template must, or inline the content \
             at the source."
        );
        let remote_stringify = render(
            "<%- stringify('secret.js') %>",
            Path::new("https://h/i.json"),
            FileAccess::Denied,
        )
        .expect_err("remote stringify");
        assert!(matches!(remote_stringify, EjsError::LocalFileRefused(_)));
        let message = remote_stringify.to_string();
        assert!(
            message.starts_with("`<%- stringify(...) %>` reads a local file")
                && message.contains("'secret.js'"),
            "{message}"
        );
    }

    #[test]
    fn has_tags_is_the_opening_delimiter() {
        assert!(has_tags("a <% b"));
        assert!(!has_tags("{\"port\": 4545}"));
        assert!(!has_tags("% > <"));
    }
}
