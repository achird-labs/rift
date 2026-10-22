//! The EJS template subset Rift's config loader evaluates, shared by the engine and `rift-lint`
//! (issue #1108) so both read a templated config the same way.
//!
//! Mountebank renders a `--configfile` with EJS 2.7.4 (through `@mbtest/mountebank-formatters`),
//! which evaluates arbitrary JavaScript. Rift has no JavaScript engine here: it evaluates the closed
//! set of tag forms Mountebank's tests and docs use (issues #1095, #1604) and refuses any other tag,
//! naming it. The forms and every refusal are documented on [`render`]. This crate has no rift
//! dependencies, which is what lets `rift-lint` use it: `rift-http-proxy` dev-depends on `rift-lint`.

use regex::Regex;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

// Fixed EJS tag patterns (issue #560): compile once at first use rather than on every
// `render` call — that runs per config file at startup, on every `POST /admin/reload`, and
// from the script CLI. All are compile-time-constant patterns, so a compile failure is a
// programming error caught immediately by tests, not a data-dependent runtime error. Each one is
// matched against a tag's body: the text between the opening `<%`/`<%-`/`<%=` and the closing
// `%>`/`-%>`, trimmed.

/// EJS 2's legacy include directive, `include path` (quoted or bare), in a `<%` or `<%-` tag.
static LEGACY_INCLUDE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^include\s+(\S+)$").expect("EJS legacy include pattern is a valid constant regex")
});

/// The include function, `include('path')`, with one quoted path.
static INCLUDE_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^include\s*\(\s*(?:'([^'\\]*)'|"([^"\\]*)")\s*\)$"#)
        .expect("EJS include call pattern is a valid constant regex")
});

/// Anything that starts like an include, to explain why one is refused.
static INCLUDE_ANY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^include\b").expect("EJS include prefix pattern is a valid constant regex")
});

/// Mountebank's `stringify(...)` and its older name `inject(...)`; the arguments are parsed by
/// [`STRINGIFY_ARGS_RE`].
static STRINGIFY_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)^(stringify|inject)\s*\((.*)\)$")
        .expect("EJS stringify call pattern is a valid constant regex")
});

/// `'file'`, `filename, 'file'` or `filename, 'file', <data>`.
static STRINGIFY_ARGS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^\s*(filename\s*,\s*)?(?:'([^'\\]*)'|"([^"\\]*)")\s*(?:,(.*))?$"#)
        .expect("EJS stringify arguments pattern is a valid constant regex")
});

/// `process.env.VAR` with an optional `|| 'default'`.
static ENV_VAR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^process\.env\.([A-Za-z_][A-Za-z0-9_]*)(?:\s*\|\|\s*['"]([^'"]*)['"]\s*)?$"#)
        .expect("EJS env-var pattern is a valid constant regex")
});

/// `data.key`, inside a file stringified with a data argument.
static DATA_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^data\.([A-Za-z_$][A-Za-z0-9_$]*)$")
        .expect("EJS data pattern is a valid constant regex")
});

/// A tag naming a function or directive that reads a local file, in any spelling: what a fetched
/// document is refused for before anything is resolved.
static FILE_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(include|stringify|inject)\b")
        .expect("EJS file-tag pattern is a valid constant regex")
});

/// The first quoted string in a tag, as the file a refused tag names.
static QUOTED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"'([^']*)'|"([^"]*)""#).expect("EJS quoted pattern is a valid constant regex")
});

/// How many files deep includes and stringifies may nest. EJS has no limit of its own: a file that
/// renders itself overflows Mountebank's stack and crashes it. Rift refuses instead.
const MAX_NESTING: usize = 16;

/// Whether EJS tags that read the local filesystem are honoured.
///
/// `--configfile` documents are authored by someone who already has filesystem access, so
/// includes and stringifies are resolved verbatim, as Mountebank resolves them — the same rationale
/// that makes rift-mock-core's `ScriptBaseDir::ConfigRelative` unrestricted. A document fetched over
/// the network (U-12's `https:` source) has no such author, so every tag that reads a file is
/// refused rather than resolved: honouring one would let whoever serves the document read arbitrary
/// local files. `process.env.X` still substitutes in both — env is deployment config the operator
/// chose to expose to their own process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAccess {
    /// Local document: `include`, `stringify` and `inject` read the files they name.
    Allowed,
    /// Remote document: `include`, `stringify` and `inject`, in every spelling, are a load error
    /// naming the tag.
    Denied,
}

/// A document with its tags rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    /// Each `process.env.VAR` tag whose variable could not be substituted: unset with no default,
    /// or set to a value that is not valid Unicode.
    pub unset_env: Vec<UnsetEnv>,
    /// Where each stringify substitution landed in `text`, so a parse error positioned in the
    /// rendered text can be traced back to the tag that produced it (issue #1279). A stringify
    /// inside a stringified file has no span: its text is escaped again before it lands.
    pub stringified: Vec<StringifiedSpan>,
}

/// One stringify substitution in [`Rendered::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringifiedSpan {
    /// The escaped file contents within the rendered text.
    pub range: std::ops::Range<usize>,
    /// The tag as written.
    pub tag: String,
    /// Where the tag is in the source, e.g. `at imposters.json:40`.
    pub place: String,
    /// Whether the tag sat between double quotes, as its output must.
    pub quoted: bool,
}

impl Rendered {
    /// Explain a parse error at 1-based `line`/`column` of [`Self::text`] when it falls inside (or
    /// just after) a stringified substitution: which tag produced the text there, and — when the
    /// tag was not written inside quotes — that it must be. `None` when the position is not in
    /// stringified text, so the caller adds nothing it cannot back up.
    #[must_use]
    pub fn explain_parse_error(&self, line: usize, column: usize) -> Option<String> {
        let line_start = if line <= 1 {
            0
        } else {
            self.text
                .match_indices('\n')
                .nth(line - 2)
                .map(|(i, _)| i + 1)?
        };
        let offset = line_start + column.saturating_sub(1);
        let span = self
            .stringified
            .iter()
            // An unquoted substitution's error often lands one byte past it; a quoted one's closing
            // quote is the author's own text, so an error there is not the tag's.
            .find(|span| {
                let end = if span.quoted {
                    span.range.end
                } else {
                    span.range.end + 1
                };
                span.range.start <= offset && offset <= end
            })?;
        let mut explained = format!(
            "the JSON error is in the text the {} tag {} inserted",
            span.tag, span.place
        );
        if !span.quoted {
            explained.push_str(&format!(
                "; stringify yields the file's contents escaped for a JSON string, so it must be \
                 written inside quotes, as in Mountebank: \"{}\"",
                span.tag
            ));
        }
        Some(explained)
    }
}

/// An environment variable a tag read but could not substitute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsetEnv {
    pub name: String,
    /// Where the tag is, as `at <file>:<line>` (with the including file for an included one).
    pub place: String,
    pub reason: EnvProblem,
}

/// Why a variable could not be substituted (issue #1116).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvProblem {
    /// Not set, and the tag gives no default, so the tag renders empty.
    Unset,
    /// Set, but the value is not valid Unicode, so the tag renders its default, or empty without one.
    NotUnicode,
}

impl UnsetEnv {
    /// One sentence naming the variable, the tag and what it rendered, shared by the engine's
    /// warning and `rift-lint`'s `W013` so both describe the condition the same way.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.reason {
            EnvProblem::Unset => format!(
                "`{}` is unset and the tag {} gives no default, so it renders empty",
                self.name, self.place
            ),
            EnvProblem::NotUnicode => format!(
                "`{}` is set but its value is not valid Unicode, so the tag {} renders its default, \
                 or empty without one",
                self.name, self.place
            ),
        }
    }
}

/// Why a document could not be rendered. Each message names the tag or file and where it is.
#[derive(Debug, thiserror::Error)]
pub enum EjsError {
    /// A tag the loader does not evaluate (issue #1095).
    #[error("{0}")]
    UnsupportedTag(String),
    /// An include or stringify in a document fetched from somewhere other than local disk.
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
    /// An include or stringify of a file that is already being rendered, which would never finish,
    /// or one nested deeper than 16 files (issue #1604). The message names the chain of files.
    #[error("{0}")]
    Recursion(String),
}

/// True when `content` holds anything that looks like a tag, so [`render`] would change or refuse it.
#[must_use]
pub fn has_tags(content: &str) -> bool {
    content.contains("<%")
}

/// Pre-process EJS tags in a config file before JSON/YAML parsing, as Mountebank's `--configfile`
/// renderer (EJS 2.7.4) would.
///
/// Evaluated, with `'` or `"` quotes and any whitespace inside the tag:
/// - `<% include file %>` (EJS 2's directive, path bare or quoted), `<%- include('file') %>` — the
///   named file, itself rendered, so includes nest. The path resolves against the directory of the
///   file the tag is in, and `.ejs` is appended to a path with no extension, as EJS does. In a
///   stringified file it resolves against the root document's directory, because Mountebank renders
///   a stringified file with the root as its EJS `filename`.
/// - `<%- stringify('file') %>`, `<%- stringify(filename, 'file') %>` and
///   `<%- stringify(filename, 'file', {key: 'value'}) %>`, and `inject(...)` for each — the named
///   file, rendered (so stringifies and includes nest), trimmed, and escaped for the inside of a JSON
///   string. A relative path always resolves against the root document's directory, at any depth,
///   as Mountebank's `stringify` does; an absolute one is read as written. The third argument is a flat object of
///   quoted strings, plain numbers, `true` and `false`; the stringified file reads it as `data`.
/// - `<%- data.key %>` / `<%= data.key %>` in a file stringified with data — the value, raw or
///   HTML-escaped. A key the object does not have renders empty, as EJS renders `undefined`.
/// - `<%= process.env.VAR %>` / `<%- process.env.VAR %>`, each with an optional `|| 'default'` —
///   the variable's value, or the default. `<%=` HTML-escapes it (`& < > " '`) exactly as EJS's
///   `escapeXML`; `<%-` inserts it raw.
///
/// Closing a tag with `-%>` drops one line break right after it, as EJS does.
///
/// Any other tag — another expression, a `<% statement %>`, `<%# comment %>`, `<%%`, `<%=` around an
/// include or stringify (which would HTML-escape the file), a `<%` with no closing `%>` — fails the
/// load, naming the tag and where it is (issue #1095). Stripping it would load a config that
/// silently differs from the file. So does `stringify('file', {...})`, which crashes Mountebank.
///
/// Each tag is substituted by its own span, so text a substitution inserts — an env var's value, a
/// data value, a stringified file — is never scanned again as if it were template. An included file
/// is rendered as template where it is included.
///
/// A file that includes or stringifies a file already being rendered (itself, or one that led to
/// it) would never finish, and nesting is capped at 16 files: both are an error naming the chain,
/// where Mountebank would overflow its stack.
///
/// A `process.env.VAR` whose variable is unset renders empty, as Mountebank's does; each one is
/// listed in [`Rendered::unset_env`] so a caller can say so. One with a `|| 'default'` is not. A
/// variable set to a value that is not valid Unicode is listed either way, because the value the
/// author set is ignored.
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
            stringified: Vec::new(),
        });
    }

    // Fail closed before any resolution: a remote document that names a local file is refused
    // outright, naming the tag.
    if file_access == FileAccess::Denied {
        refuse_local_file_tags(content, config_path)?;
    }

    // A path with no parent (`""`, `/`) has nothing to resolve against but the working directory.
    let root_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let mut renderer = Renderer {
        root: config_path,
        root_dir,
        file_access,
        unset_env: Vec::new(),
    };
    let chain = [Link {
        key: lexical_normalize(config_path),
        shown: config_path.display().to_string(),
    }];
    let locate =
        |offset: usize| format!("at {}:{}", config_path.display(), line_of(content, offset));
    let frame = Frame {
        text: content,
        include_base: config_path,
        data: None,
        chain: &chain,
        locate: &locate,
    };
    let mut text = String::with_capacity(content.len());
    let mut stringified = Vec::new();
    renderer.render_frame(&frame, &mut text, Some(&mut stringified))?;
    Ok(Rendered {
        text,
        unset_env: renderer.unset_env,
        stringified,
    })
}

/// Refuse the first tag in a fetched document that reads a local file, in any spelling.
fn refuse_local_file_tags(content: &str, config_path: &Path) -> Result<(), EjsError> {
    let mut from = 0;
    while let Some(found) = content[from..].find("<%") {
        let offset = from + found;
        // An unterminated tag runs to the end of the document: still scanned, so it fails closed.
        let close = content[offset + 2..].find("%>");
        let end = close.map_or(content.len(), |close| offset + 2 + close + 2);
        let tag = &content[offset..end];
        if let Some(keyword) = FILE_TAG_RE.captures(tag).and_then(|cap| cap.get(1)) {
            let label = match keyword.as_str() {
                "include" => "<% include ... %>",
                "stringify" => "<%- stringify(...) %>",
                _ => "<%- inject(...) %>",
            };
            let named = QUOTED_RE
                .captures(tag)
                .and_then(|cap| cap.get(1).or_else(|| cap.get(2)))
                .map(|m| m.as_str())
                .or_else(|| close.and_then(|_| legacy_include(parse_tag(tag).body)))
                .unwrap_or(tag);
            return Err(EjsError::LocalFileRefused(format!(
                "`{label}` reads a local file and is not honoured in a document fetched from {} — \
                 it names '{named}'. Only local `--configfile` documents may include local files; \
                 use `--configfile` if the template must, or inline the content at the source.",
                config_path.display(),
            )));
        }
        from = end;
    }
    Ok(())
}

/// The values of a stringify's data argument, each already in the form JavaScript's `String(v)`
/// gives it, which is what both `<%-` and `<%=` print.
type Data = BTreeMap<String, String>;

/// One file in the chain of includes and stringifies being rendered, outermost first.
#[derive(Debug, Clone)]
struct Link {
    /// The resolved path, lexically normalised, to recognise a file already in the chain.
    key: PathBuf,
    /// The path as the tag wrote it (the root's as given), for the error naming the chain.
    shown: String,
}

/// One template being rendered: the root document, an included file or a stringified one.
struct Frame<'f> {
    text: &'f str,
    /// The file whose directory an include in `text` resolves against: EJS's `filename` option.
    include_base: &'f Path,
    /// What `data` is in this file; `None` where it is undefined.
    data: Option<&'f Data>,
    /// The files being rendered, this one last.
    chain: &'f [Link],
    /// Turns an offset in `text` into the place named in an error, e.g. `at b.json:3, included at
    /// imposters.ejs:2`.
    locate: &'f dyn Fn(usize) -> String,
}

struct Renderer<'r> {
    /// The root document, which every stringify resolves against.
    root: &'r Path,
    root_dir: &'r Path,
    file_access: FileAccess,
    unset_env: Vec<UnsetEnv>,
}

impl Renderer<'_> {
    /// Render `frame` onto `out`. `spans` is `Some` when `out` is the final text (the document and
    /// files included into it), so stringify substitutions there are recorded by position.
    fn render_frame(
        &mut self,
        frame: &Frame<'_>,
        out: &mut String,
        mut spans: Option<&mut Vec<StringifiedSpan>>,
    ) -> Result<(), EjsError> {
        let text = frame.text;
        let mut from = 0;
        while let Some(found) = text[from..].find("<%") {
            let offset = from + found;
            out.push_str(&text[from..offset]);
            let Some(close) = text[offset + 2..].find("%>") else {
                return Err(self.unsupported(&text[offset..], false, "", &(frame.locate)(offset)));
            };
            let end = offset + 2 + close + 2;
            let written = &text[offset..end];
            let tag = parse_tag(written);
            let action = classify(&tag)
                .map_err(|note| self.unsupported(written, true, note, &(frame.locate)(offset)))?;
            match action {
                Action::Env {
                    name,
                    default,
                    escape,
                } => {
                    let value = env_value(name, default);
                    if let Some(reason) = value.problem {
                        self.unset_env.push(UnsetEnv {
                            name: name.to_string(),
                            place: (frame.locate)(offset),
                            reason,
                        });
                    }
                    push_value(out, &value.value, escape);
                }
                Action::DataValue { key, escape } => {
                    let Some(data) = frame.data else {
                        return Err(self.unsupported(
                            written,
                            true,
                            NOTE_NO_DATA,
                            &(frame.locate)(offset),
                        ));
                    };
                    // A key the object lacks is `undefined`, which EJS prints as empty through
                    // both tags (Domain-optional: absence is a value of the template language).
                    push_value(out, data.get(key).map_or("", String::as_str), escape);
                }
                Action::Include(file) => {
                    let place = (frame.locate)(offset);
                    let path = resolve_include(file, frame.include_base);
                    let chain = enter(frame.chain, &path, file, &place)?;
                    let contents =
                        std::fs::read_to_string(&path).map_err(|io| EjsError::Include {
                            file: file.to_string(),
                            path: path.clone(),
                            io,
                        })?;
                    // EJS strips a byte-order mark from a file it includes.
                    let contents = contents.strip_prefix('\u{FEFF}').unwrap_or(&contents);
                    let locate = |at: usize| {
                        format!("at {file}:{}, included {place}", line_of(contents, at))
                    };
                    let included = Frame {
                        text: contents,
                        include_base: &path,
                        data: frame.data,
                        chain: &chain,
                        locate: &locate,
                    };
                    self.render_frame(&included, out, spans.as_deref_mut())?;
                }
                Action::Stringify { file, data } => {
                    let place = (frame.locate)(offset);
                    // An absolute path is read as written, as Rift has always read it (the TLS and
                    // Kubernetes guides stringify `/certs/...`). Mountebank's `path.join` would read
                    // it under the root's directory instead.
                    let path = self.root_dir.join(file);
                    let chain = enter(frame.chain, &path, file, &place)?;
                    let contents =
                        std::fs::read_to_string(&path).map_err(|io| EjsError::Stringify {
                            file: file.to_string(),
                            path: path.clone(),
                            io,
                        })?;
                    let locate = |at: usize| {
                        format!("at {file}:{}, stringified {place}", line_of(&contents, at))
                    };
                    let stringified = Frame {
                        text: &contents,
                        include_base: self.root,
                        data: data.as_ref(),
                        chain: &chain,
                        locate: &locate,
                    };
                    let mut rendered = String::new();
                    self.render_frame(&stringified, &mut rendered, None)?;
                    let start = out.len();
                    out.push_str(&json_string_inner(&rendered, file)?);
                    if let Some(spans) = spans.as_deref_mut() {
                        spans.push(StringifiedSpan {
                            range: start..out.len(),
                            tag: written.to_string(),
                            place,
                            quoted: text[..offset].ends_with('"') && text[end..].starts_with('"'),
                        });
                    }
                }
            }
            from = end;
            if tag.trim_line_break {
                from += leading_line_break_len(&text[from..]);
            }
        }
        out.push_str(&text[from..]);
        Ok(())
    }

    fn unsupported(&self, text: &str, terminated: bool, note: &str, place: &str) -> EjsError {
        EjsError::UnsupportedTag(
            UnsupportedTag {
                text,
                terminated,
                note,
            }
            .message(place, self.file_access),
        )
    }
}

/// The chain with `path` appended, or the error when `path` is already in it or the chain is too
/// deep. `written` is the path as the tag wrote it, and `place` where the tag is.
fn enter(chain: &[Link], path: &Path, written: &str, place: &str) -> Result<Vec<Link>, EjsError> {
    let key = lexical_normalize(path);
    let already = chain.iter().any(|link| link.key == key);
    let mut next = chain.to_vec();
    next.push(Link {
        key,
        shown: written.to_string(),
    });
    let shown = || {
        next.iter()
            .map(|link| link.shown.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    };
    if already {
        return Err(EjsError::Recursion(format!(
            "EJS file '{written}' {place} is already being rendered, so rendering it again would \
             never finish: {}",
            shown()
        )));
    }
    // The root document is not nested.
    if next.len() - 1 > MAX_NESTING {
        return Err(EjsError::Recursion(format!(
            "EJS includes and stringifies nest deeper than {MAX_NESTING} files {place}: {}",
            shown()
        )));
    }
    Ok(next)
}

/// `path` with `.` and `name/..` components folded away, without touching the filesystem: the same
/// file reached as `a.json` and `./a.json` is one link in the chain. A cycle spelled through a
/// symlink is not recognised, and is stopped by the depth limit instead.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir
                if matches!(out.components().next_back(), Some(Component::Normal(_))) =>
            {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Where an include of `file` from the template `base` reads, by EJS 2.7.4's `getIncludePath`: an
/// absolute path as written, otherwise relative to `base`'s directory; either way with `.ejs`
/// appended when the path has no extension.
fn resolve_include(file: &str, base: &Path) -> PathBuf {
    let written = Path::new(file);
    let mut path = if written.is_absolute() || file.starts_with('/') {
        written.to_path_buf()
    } else {
        // A path with no parent (`""`, `/`) has nothing to resolve against but the working
        // directory.
        base.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(written)
    };
    if written.extension().is_none() {
        path.as_mut_os_string().push(".ejs");
    }
    path
}

/// `rendered`, trimmed as Mountebank's `stringify` trims it (JavaScript's `String.prototype.trim`),
/// JSON-encoded without the surrounding quotes. The template supplies the quotes (`"inject":
/// "<%- stringify('inject.js') %>"`), so only the escaped inner content is returned — Mountebank's
/// contract too (issue #355 Item 7, #1279).
fn json_string_inner(rendered: &str, file: &str) -> Result<String, EjsError> {
    // JavaScript's whitespace: Unicode's White_Space without NEL, plus the byte-order mark.
    let trimmed =
        rendered.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}');
    let json_quoted = serde_json::to_string(trimmed).map_err(|json| EjsError::Encode {
        file: file.to_string(),
        json,
    })?;
    // `to_string` of a `str` always yields a quoted JSON string.
    Ok(json_quoted[1..json_quoted.len() - 1].to_string())
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

/// The length of the one line break (`\r\n`, `\n` or `\r`) EJS drops after a `-%>`, if `rest`
/// starts with one.
fn leading_line_break_len(rest: &str) -> usize {
    if rest.starts_with("\r\n") {
        2
    } else if rest.starts_with(['\n', '\r']) {
        1
    } else {
        0
    }
}

/// How a tag opens, which decides what its body may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// `<%`: a statement. Only EJS 2's `include` directive is evaluated.
    Scriptlet,
    /// `<%-`: an expression, inserted raw.
    Raw,
    /// `<%=`: an expression, HTML-escaped.
    Escaped,
    /// `<%#`, `<%_`, `<%%`: never evaluated.
    Other,
}

/// A terminated tag, split into its parts.
#[derive(Debug)]
struct ParsedTag<'t> {
    opening: Opening,
    /// The text between the opening and closing delimiters, trimmed.
    body: &'t str,
    /// Whether it closes with `-%>`.
    trim_line_break: bool,
}

/// Split `tag`, which runs from `<%` through `%>`.
fn parse_tag(tag: &str) -> ParsedTag<'_> {
    let inner = &tag[2..tag.len() - 2];
    let (opening, inner) = match inner.as_bytes().first() {
        Some(b'=') => (Opening::Escaped, &inner[1..]),
        Some(b'-') => (Opening::Raw, &inner[1..]),
        Some(b'#' | b'_' | b'%') => (Opening::Other, inner),
        _ => (Opening::Scriptlet, inner),
    };
    let (body, trim_line_break) = match inner.strip_suffix('-') {
        Some(body) => (body, true),
        None => (inner, false),
    };
    ParsedTag {
        opening,
        body: body.trim(),
        trim_line_break,
    }
}

/// What an evaluated tag does.
#[derive(Debug)]
enum Action<'t> {
    Include(&'t str),
    Stringify {
        file: &'t str,
        data: Option<Data>,
    },
    Env {
        name: &'t str,
        default: Option<&'t str>,
        escape: bool,
    },
    DataValue {
        key: &'t str,
        escape: bool,
    },
}

const NOTE_ESCAPED_FILE: &str =
    " (`<%=` would HTML-escape the file's text, which EJS does too; write `<%-`)";
const NOTE_INCLUDE_ARGS: &str =
    " (an include takes one quoted path; an include with a data argument is not evaluated)";
const NOTE_STRINGIFY_ARGS: &str = " (stringify takes `'file'`, `filename, 'file'` or \
                                   `filename, 'file', {...}`)";
const NOTE_TWO_ARG_OBJECT: &str = " (Mountebank's renderer fails on this form too: with two \
                                   arguments the object is taken as the file name; write \
                                   `stringify(filename, 'file', {...})`)";
const NOTE_DATA: &str = " (a data argument is evaluated only as a flat object of quoted strings, \
                         plain numbers, `true` and `false`)";
const NOTE_NO_DATA: &str = " (`data` is only set inside a file stringified with a data argument, \
                            as `stringify(filename, 'file', {...})`)";

/// What `tag` does, or the note explaining its refusal (empty when there is nothing to add).
fn classify<'t>(tag: &ParsedTag<'t>) -> Result<Action<'t>, &'static str> {
    let body = tag.body;
    let escape = match tag.opening {
        Opening::Other => return Err(""),
        Opening::Scriptlet => return legacy_include(body).map(Action::Include).ok_or(""),
        Opening::Escaped => true,
        Opening::Raw => false,
    };
    if let Some(cap) = ENV_VAR_RE.captures(body)
        && let Some(name) = cap.get(1)
    {
        return Ok(Action::Env {
            name: name.as_str(),
            default: cap.get(2).map(|m| m.as_str()),
            escape,
        });
    }
    if let Some(key) = DATA_RE.captures(body).and_then(|cap| cap.get(1)) {
        return Ok(Action::DataValue {
            key: key.as_str(),
            escape,
        });
    }
    if INCLUDE_ANY_RE.is_match(body) {
        if escape {
            return Err(NOTE_ESCAPED_FILE);
        }
        return INCLUDE_CALL_RE
            .captures(body)
            .and_then(|cap| cap.get(1).or_else(|| cap.get(2)))
            .map(|m| m.as_str())
            .or_else(|| legacy_include(body))
            .map(Action::Include)
            .ok_or(NOTE_INCLUDE_ARGS);
    }
    if let Some(args) = STRINGIFY_CALL_RE.captures(body).and_then(|cap| cap.get(2)) {
        if escape {
            return Err(NOTE_ESCAPED_FILE);
        }
        return stringify_action(args.as_str());
    }
    Err("")
}

/// The path of an EJS 2 `include path` directive, its quotes (if any) removed.
fn legacy_include(body: &str) -> Option<&str> {
    let path = LEGACY_INCLUDE_RE.captures(body)?.get(1)?.as_str();
    Some(
        ['\'', '"']
            .iter()
            .find_map(|q| path.strip_prefix(*q).and_then(|p| p.strip_suffix(*q)))
            .unwrap_or(path),
    )
}

fn stringify_action(args: &str) -> Result<Action<'_>, &'static str> {
    let cap = STRINGIFY_ARGS_RE
        .captures(args)
        .ok_or(NOTE_STRINGIFY_ARGS)?;
    let file = cap
        .get(2)
        .or_else(|| cap.get(3))
        .ok_or(NOTE_STRINGIFY_ARGS)?
        .as_str();
    let Some(rest) = cap.get(4) else {
        return Ok(Action::Stringify { file, data: None });
    };
    if cap.get(1).is_none() {
        // `stringify('file', x)`: Mountebank reads `x` as the file name.
        return Err(if rest.as_str().trim_start().starts_with('{') {
            NOTE_TWO_ARG_OBJECT
        } else {
            NOTE_STRINGIFY_ARGS
        });
    }
    let data = parse_data(rest.as_str()).ok_or(NOTE_DATA)?;
    Ok(Action::Stringify {
        file,
        data: Some(data),
    })
}

/// A flat JavaScript object literal: `{key: 'string', "k": 12, k2: true}`, keys bare or quoted, a
/// trailing comma allowed. `None` for anything else.
fn parse_data(src: &str) -> Option<Data> {
    let mut p = Literal {
        rest: src.trim_start(),
    };
    p.expect('{')?;
    let mut data = Data::new();
    loop {
        p.skip_ws();
        if p.expect('}').is_some() {
            break;
        }
        let key = p.key()?;
        p.skip_ws();
        p.expect(':')?;
        p.skip_ws();
        let value = p.value()?;
        // A repeated key keeps its last value, as in JavaScript.
        data.insert(key, value);
        p.skip_ws();
        if p.expect(',').is_none() {
            p.expect('}')?;
            break;
        }
    }
    p.skip_ws();
    p.rest.is_empty().then_some(data)
}

/// A cursor over a JavaScript literal.
struct Literal<'s> {
    rest: &'s str,
}

impl<'s> Literal<'s> {
    fn skip_ws(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn expect(&mut self, c: char) -> Option<()> {
        self.rest = self.rest.strip_prefix(c)?;
        Some(())
    }

    fn identifier(&mut self) -> Option<&'s str> {
        let end = self
            .rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(self.rest.len());
        let (ident, rest) = self.rest.split_at(end);
        if ident.is_empty() || ident.starts_with(|c: char| c.is_ascii_digit()) {
            return None;
        }
        self.rest = rest;
        Some(ident)
    }

    fn key(&mut self) -> Option<String> {
        if self.rest.starts_with(['\'', '"']) {
            self.string()
        } else {
            self.identifier().map(str::to_string)
        }
    }

    /// A value as JavaScript's `String(value)` prints it.
    fn value(&mut self) -> Option<String> {
        if self.rest.starts_with(['\'', '"']) {
            return self.string();
        }
        if self
            .rest
            .starts_with(|c: char| c == '-' || c.is_ascii_digit())
        {
            return self.number();
        }
        let word = self.identifier()?;
        matches!(word, "true" | "false").then(|| word.to_string())
    }

    /// A quoted string with the common escapes; any other escape, or a line break, is `None`.
    fn string(&mut self) -> Option<String> {
        let quote = self.rest.chars().next()?;
        let mut out = String::new();
        let mut chars = self.rest[1..].char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                c if c == quote => {
                    self.rest = &self.rest[1 + i + 1..];
                    return Some(out);
                }
                '\\' => out.push(match chars.next()?.1 {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'v' => '\u{b}',
                    e @ ('\\' | '\'' | '"') => e,
                    _ => return None,
                }),
                '\n' | '\r' => return None,
                c => out.push(c),
            }
        }
        None
    }

    /// A plain decimal number (`12`, `-0.5`), printed as JavaScript prints it. Exponents, and
    /// magnitudes JavaScript would print with one, are `None`.
    fn number(&mut self) -> Option<String> {
        static NUMBER_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?")
                .expect("EJS data number pattern is a valid constant regex")
        });
        let literal = NUMBER_RE.find(self.rest)?.as_str();
        let after = &self.rest[literal.len()..];
        if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '.' || c == '_') {
            return None;
        }
        let value: f64 = literal.parse().ok()?;
        let magnitude = value.abs();
        if magnitude >= 1e21 || (magnitude != 0.0 && magnitude < 1e-6) {
            return None;
        }
        self.rest = after;
        // Rust prints the shortest round-tripping decimal, as JavaScript does in this range; only
        // negative zero differs (`String(-0)` is `0`).
        Some(if value == 0.0 {
            "0".to_string()
        } else {
            value.to_string()
        })
    }
}

/// Append `value`, HTML-escaped when the tag is `<%=`.
fn push_value(out: &mut String, value: &str, escape: bool) {
    if escape {
        out.push_str(&escape_xml(value));
    } else {
        out.push_str(value);
    }
}

/// EJS 2.7.4's `escapeXML`: `& < > " '` as `&amp; &lt; &gt; &#34; &#39;`.
fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A `process.env.VAR` tag's value.
struct EnvValue {
    value: String,
    /// Why the variable could not be substituted, when it could not.
    problem: Option<EnvProblem>,
}

fn env_value(name: &str, default: Option<&str>) -> EnvValue {
    match (std::env::var(name), default) {
        (Ok(value), _) => EnvValue {
            value,
            problem: None,
        },
        (Err(std::env::VarError::NotPresent), Some(default)) => EnvValue {
            value: default.to_string(),
            problem: None,
        },
        (Err(std::env::VarError::NotPresent), None) => EnvValue {
            value: String::new(),
            problem: Some(EnvProblem::Unset),
        },
        (Err(std::env::VarError::NotUnicode(_)), default) => EnvValue {
            value: default.unwrap_or_default().to_string(),
            problem: Some(EnvProblem::NotUnicode),
        },
    }
}

/// An EJS tag the preprocessor found and does not evaluate.
#[derive(Debug)]
struct UnsupportedTag<'a> {
    /// The tag as written, from `<%` through `%>` (or to the end of the text when unterminated).
    text: &'a str,
    terminated: bool,
    /// Why a tag that looks supported is not, where that applies.
    note: &'a str,
}

impl UnsupportedTag<'_> {
    const MAX_SHOWN_CHARS: usize = 80;

    /// The load error, with the tag found `place`.
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
                 loaded. Only `<%= process.env.VAR %>` and `<%- process.env.VAR %>` (each with an \
                 optional `|| 'default'`), `<% include file %>`, `<%- include('file') %>`, \
                 `<%- stringify(filename, 'file') %>` (with an optional data object, or as \
                 `stringify('file')` or `inject(...)`) and, in a stringified file, \
                 `<%- data.key %>` are evaluated. If the tag is meant literally, load the file \
                 with --no-parse (a --configfile or file: source)."
            ),
            FileAccess::Denied => format!(
                "unsupported EJS tag `{shown}`{unterminated} {place}{note}, so the document was not \
                 loaded. Only `<%= process.env.VAR %>` and `<%- process.env.VAR %>` (each with an \
                 optional `|| 'default'`) are evaluated in a fetched document, and it is always \
                 preprocessed: remove the tag at the source."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ejs_statics_match_their_tags() {
        assert_eq!(legacy_include("include 'a/b.json'"), Some("a/b.json"));
        assert_eq!(legacy_include("include bare.json"), Some("bare.json"));
        assert_eq!(legacy_include("include('x')"), None);

        let call = INCLUDE_CALL_RE.captures(r#"include( "a.json" )"#).unwrap();
        assert_eq!(&call[2], "a.json");

        let stringify = STRINGIFY_CALL_RE
            .captures("inject(filename, 'f.js')")
            .unwrap();
        assert_eq!(&stringify[1], "inject");
        let args = STRINGIFY_ARGS_RE.captures(&stringify[2]).unwrap();
        assert!(args.get(1).is_some(), "the filename argument");
        assert_eq!(&args[2], "f.js");
        assert!(args.get(4).is_none());

        let env_cap = ENV_VAR_RE.captures("process.env.PORT || '4545'").unwrap();
        assert_eq!(env_cap[1].to_string(), "PORT");
        assert_eq!(env_cap[2].to_string(), "4545");
        assert!(
            ENV_VAR_RE
                .captures("process.env.HOST")
                .unwrap()
                .get(2)
                .is_none()
        );
        assert!(ENV_VAR_RE.captures("someOtherExpr()").is_none());

        assert_eq!(&DATA_RE.captures("data.testValue").unwrap()[1], "testValue");
    }

    #[test]
    fn tags_split_into_opening_body_and_trim() {
        let tag = parse_tag("<%- include('a') -%>");
        assert_eq!(tag.opening, Opening::Raw);
        assert_eq!(tag.body, "include('a')");
        assert!(tag.trim_line_break);
        let tag = parse_tag("<%=\n process.env.X\n%>");
        assert_eq!(tag.opening, Opening::Escaped);
        assert_eq!(tag.body, "process.env.X");
        assert!(!tag.trim_line_break);
        assert_eq!(parse_tag("<%# c %>").opening, Opening::Other);
        assert_eq!(parse_tag("<%% x %>").opening, Opening::Other);
        assert_eq!(parse_tag("<% include f %>").opening, Opening::Scriptlet);
        assert!(
            !parse_tag("<%-%>").trim_line_break,
            "the dash is the opening's"
        );
    }

    #[test]
    fn data_literals_parse_as_javascript_prints_them() {
        let data =
            parse_data("{a: 'x', \"b\": 12, 'c': -0.50, d: true, e: false, f: -0,}").unwrap();
        let get = |k: &str| data.get(k).map(String::as_str);
        assert_eq!(get("a"), Some("x"));
        assert_eq!(get("b"), Some("12"));
        assert_eq!(get("c"), Some("-0.5"));
        assert_eq!(get("d"), Some("true"));
        assert_eq!(get("e"), Some("false"));
        assert_eq!(get("f"), Some("0"));
        assert_eq!(parse_data("{}").unwrap().len(), 0);
        for bad in [
            "",
            "{",
            "{a}",
            "{a: }",
            "{1a: 1}",
            "{a: 1} x",
            "{a: 01}",
            "{a: 1.}",
            "{a: null}",
        ] {
            assert!(parse_data(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn escape_xml_matches_ejs() {
        assert_eq!(escape_xml("a&b<c>\"d'e"), "a&amp;b&lt;c&gt;&#34;d&#39;e");
        assert_eq!(escape_xml("plain"), "plain");
    }

    #[test]
    fn lexical_normalize_folds_dot_components() {
        assert_eq!(
            lexical_normalize(Path::new("/a/./b/../c.json")),
            Path::new("/a/c.json")
        );
        assert_eq!(lexical_normalize(Path::new("../x")), Path::new("../x"));
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
                reason: EnvProblem::Unset,
            }]
        );
        assert_eq!(
            rendered.unset_env[0].describe(),
            format!(
                "`{UNSET}` is unset and the tag at /cfg/imposters.json:2 gives no default, so it \
                 renders empty"
            )
        );
    }

    // Issue #1116: a variable that is set but not valid Unicode was reported as "unset" without a
    // default, and not reported at all with one, though the author set a value that was ignored.
    #[cfg(unix)]
    #[test]
    fn a_set_but_non_unicode_variable_is_reported_as_such() {
        use std::os::unix::ffi::OsStrExt;
        const NOT_UNICODE: &str = "RIFT_EJS_TEST_1116_NOT_UNICODE";
        // Safety: a name no other test reads, set once and never removed.
        unsafe { std::env::set_var(NOT_UNICODE, std::ffi::OsStr::from_bytes(b"\xff\xfe")) };

        for (tag, rendered_text) in [
            (format!("<%= process.env.{NOT_UNICODE} %>"), ""),
            (
                format!("<%= process.env.{NOT_UNICODE} || 'fallback' %>"),
                "fallback",
            ),
        ] {
            let rendered =
                render(&tag, Path::new("/cfg/a.json"), FileAccess::Allowed).expect("renders");
            assert_eq!(rendered.text, rendered_text, "{tag}");
            assert_eq!(
                rendered.unset_env,
                vec![UnsetEnv {
                    name: NOT_UNICODE.to_string(),
                    place: "at /cfg/a.json:1".to_string(),
                    reason: EnvProblem::NotUnicode,
                }],
                "{tag}"
            );
            assert!(
                rendered.unset_env[0]
                    .describe()
                    .contains("is set but its value is not valid Unicode"),
                "{}",
                rendered.unset_env[0].describe()
            );
        }
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

    // ===== Issue #1279 =====

    fn render_with(fixture: &str, doc: &str) -> Rendered {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.json"), fixture).unwrap();
        let path = dir.path().join("doc.json");
        render(doc, &path, FileAccess::Allowed).expect("renders")
    }

    #[test]
    fn stringify_trims_like_mountebank() {
        let rendered = render_with("  {\"a\":1}\n\n", r#"{"b": "<%- stringify('f.json') %>"}"#);
        assert_eq!(rendered.text, r#"{"b": "{\"a\":1}"}"#);
    }

    #[test]
    fn stringified_spans_record_the_tag_and_whether_it_is_quoted() {
        let rendered = render_with(
            "v",
            "{\"q\": \"<%- stringify('f.json') %>\",\n\"u\": <%- stringify('f.json') %>}",
        );
        assert_eq!(rendered.stringified.len(), 2);
        assert!(rendered.stringified[0].quoted);
        assert!(!rendered.stringified[1].quoted);
        assert_eq!(rendered.stringified[1].tag, "<%- stringify('f.json') %>");
        assert!(
            rendered.stringified[1].place.ends_with("doc.json:2"),
            "{}",
            rendered.stringified[1].place
        );
        assert_eq!(&rendered.text[rendered.stringified[1].range.clone()], "v");
    }

    #[test]
    fn explain_parse_error_maps_a_position_to_the_tag() {
        let rendered = render_with("{\"a\":1}", "{\"x\":1,\n\"b\": <%- stringify('f.json') %>}");
        // Line 2 is `"b": {\"a\":1}}`; serde reports `key must be a string` at the backslash.
        let column = "\"b\": {\\".len();
        let explained = rendered
            .explain_parse_error(2, column)
            .expect("the position is inside the stringified text");
        assert!(
            explained.contains("<%- stringify('f.json') %>"),
            "{explained}"
        );
        assert!(
            explained.contains("must be written inside quotes"),
            "{explained}"
        );
        assert_eq!(
            rendered.explain_parse_error(1, 3),
            None,
            "line 1 is nowhere near it"
        );
    }

    #[test]
    fn an_untagged_document_records_no_spans() {
        let dir = tempfile::tempdir().unwrap();
        let rendered = render("{}", &dir.path().join("d.json"), FileAccess::Allowed).unwrap();
        assert!(rendered.stringified.is_empty());
        assert_eq!(rendered.explain_parse_error(1, 1), None);
    }
}
