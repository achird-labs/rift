//! Imposter sources (U-12): load imposters from a URI instead of only from a local path.
//!
//! A source is a scheme plus a way to turn a [`SourceRef`] into imposter configs. Two are built
//! in — `file:` and `https:` — and embedders register their own with
//! [`crate::server::ServerBuilder::imposter_source`]; that is how the cluster `git:`/`s3:`/
//! `registry:` providers attach without this crate knowing they exist.
//!
//! Two properties are load-bearing and easy to lose in a refactor:
//!
//! 1. **Parsing is shared.** A provider hands back *bytes*; the document shapes, format sniffing
//!    and block rules all run in [`crate::config_loader`], so `file:` and `https:` cannot drift
//!    into two dialects of the same config format.
//! 2. **`--configfile` is unchanged.** It is sugar for `--imposters file:<path>`, and
//!    [`FileSource`] delegates to the very same loader function the flag used before, so the
//!    behaviour is identical rather than merely similar.

use crate::config_loader::{self, ConfigSource, LoadedConfig};
use crate::front_door::RouteTable;
use crate::imposter::ImposterConfig;
use crate::intercept_control::InterceptStartOptions;
// `.context(…)` keeps the original error as `source()`. `anyhow!("…: {e}")` does not — it renders
// the cause into a string and returns a fresh, sourceless error, which is why every wrap in this
// file with a cause worth keeping uses `Context` (issue #951). The `map_err(|_| …)` wraps on the
// poisoned-mutex paths are the deliberate exception: a `PoisonError` carries nothing to keep.
use anyhow::Context as _;
use rift_mock_core::proxy::OutboundTls;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// Refuse a body larger than this. A config document is small; anything at this scale is a
/// misconfigured URL (or a hostile one), and it would otherwise be buffered whole.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Whole-request budget for an `https:` fetch — connect, headers and body.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// A source URI as written on the command line: `file:mocks.json`, `https://host/imposters.json`,
/// or a bare path (sugar for `file:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub uri: String,
}

impl SourceRef {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    /// The scheme this ref dispatches on.
    ///
    /// A bare path is `file`, so `--imposters mocks.json` works like `--configfile mocks.json`.
    /// The `://` form is checked before the bare `scheme:` form so that a compound scheme such as
    /// `git+https://…` (a cluster provider) resolves to `git+https` rather than to `git`.
    ///
    /// The bare `scheme:` form dispatches too — `s3:key` is scheme `s3`, not a file named
    /// `s3:key` — but only when what precedes the colon is actually scheme-shaped; see
    /// [`is_scheme`]. Anything else is a path, and paths are `file`.
    pub fn scheme(&self) -> &str {
        match self.uri.split_once("://") {
            Some((scheme, _)) => scheme,
            None => match self.uri.split_once(':') {
                Some((scheme, _)) if is_scheme(scheme) => scheme,
                _ => "file",
            },
        }
    }

    /// The scheme-specific part: what `file:` should open, or the whole URI for network schemes
    /// that need it verbatim.
    pub fn target(&self) -> &str {
        self.uri.strip_prefix("file:").unwrap_or(&self.uri)
    }
}

/// Is `s` shaped like a URI scheme, for the purpose of reading `scheme:rest` as a scheme rather
/// than as a path that happens to contain a colon?
///
/// RFC 3986 §3.1 — `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )` — with one deliberate deviation:
/// the length must exceed 1, so a Windows drive-letter path (`C:\mocks.json`) stays a `file:`
/// path instead of dispatching to a scheme named `C` that no provider will ever be registered
/// for. Single-letter schemes are RFC-legal; none exist here, and keeping drive letters working
/// is worth more than reserving them.
fn is_scheme(s: &str) -> bool {
    s.len() > 1
        && s.as_bytes()[0].is_ascii_alphabetic()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// What a source knows about the version it just served, used to skip redundant work.
#[derive(Debug, Clone)]
pub struct SourceMeta {
    /// ETag, commit sha, content hash — whatever the provider has. `None` means "no version
    /// information", which is always treated as changed.
    pub version: Option<String>,
    pub fetched_at: SystemTime,
}

/// The result of a fetch.
#[derive(Debug)]
pub struct FetchedImposters {
    pub configs: Vec<ImposterConfig>,
    /// The document's optional `intercept` block (issue #655). Carried here — not in the original
    /// U-12 sketch — because `--configfile` is now sugar for `--imposters file:<path>`, and a
    /// configfile that declares a block must keep starting its listener.
    pub intercept: Option<InterceptStartOptions>,
    /// The document's optional `routes` block (issue #19 / U-11). Same reasoning as `intercept`.
    pub routes: Option<RouteTable>,
    pub meta: SourceMeta,
    /// True when the provider proved the content had not changed since its last fetch (an HTTP
    /// 304, a matching commit sha) and `configs` therefore came from its cache without being
    /// re-parsed.
    ///
    /// Not in the original U-12 sketch, but the sketch had no way to express "nothing changed",
    /// and without it every poll re-applies an identical config — which is exactly the log growth
    /// and state churn the ETag support exists to avoid.
    pub unchanged: bool,
}

/// A scheme handler. Implementors fetch bytes and hand back parsed configs; they must route the
/// parsing through [`config_loader`] rather than parsing themselves.
pub trait ImposterSource: Send + Sync {
    /// The schemes this source claims. Registering two sources for one scheme is a startup error.
    fn schemes(&self) -> &'static [&'static str];

    fn fetch<'a>(
        &'a self,
        r: &'a SourceRef,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<FetchedImposters>> + Send + 'a>>;
}

/// `file:` — the local filesystem, and the path `--configfile` takes.
#[derive(Debug, Clone)]
pub struct FileSource {
    /// Mirrors `--no-parse`: skip EJS preprocessing.
    pub no_parse: bool,
}

impl FileSource {
    pub fn new(no_parse: bool) -> Self {
        Self { no_parse }
    }
}

impl ImposterSource for FileSource {
    fn schemes(&self) -> &'static [&'static str] {
        &["file"]
    }

    fn fetch<'a>(
        &'a self,
        r: &'a SourceRef,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<FetchedImposters>> + Send + 'a>> {
        Box::pin(async move {
            let path = PathBuf::from(r.target());
            let no_parse = self.no_parse;
            // The same call `--configfile` made before U-12 — including the EJS pass, the
            // config-relative script base, and the block rules. Not a reimplementation.
            let loaded: LoadedConfig = tokio::task::spawn_blocking(move || {
                config_loader::load_configs_full(&ConfigSource::File { path, no_parse })
            })
            .await??;
            Ok(FetchedImposters {
                configs: loaded.imposters,
                intercept: loaded.intercept,
                routes: loaded.routes,
                meta: SourceMeta {
                    version: None,
                    fetched_at: SystemTime::now(),
                },
                unchanged: false,
            })
        })
    }
}

/// What [`HttpSource`] remembers per URI so a later fetch can be conditional.
#[derive(Debug, Clone)]
struct CachedResponse {
    etag: String,
    configs: Vec<ImposterConfig>,
}

/// `http:`/`https:` — fetch a document over HTTP, honouring `ETag`/`If-None-Match`.
#[derive(Debug)]
pub struct HttpSource {
    client: reqwest::Client,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

impl HttpSource {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_timeout(DEFAULT_HTTP_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> anyhow::Result<Self> {
        Self::with_timeout_and_policy(timeout, &OutboundTls::default())
    }

    /// A source that fetches under `policy` (issue #974).
    ///
    /// `--configfile https://…` reaches an origin like any other outbound call, so a config
    /// document served behind a privately-issued CA needs the same trust policy the proxy stubs
    /// use — otherwise the operator's `--upstream-ca-file` fixes their proxying and leaves the
    /// config fetch failing with the identical `UnknownIssuer`.
    pub fn with_policy(policy: &OutboundTls) -> anyhow::Result<Self> {
        Self::with_timeout_and_policy(DEFAULT_HTTP_TIMEOUT, policy)
    }

    pub fn with_timeout_and_policy(
        timeout: Duration,
        policy: &OutboundTls,
    ) -> anyhow::Result<Self> {
        let client = policy
            .reqwest_builder()?
            .timeout(timeout)
            // A redirect is followed only while it stays on http(s). reqwest would reject an
            // exotic scheme on its own, but a config source is exactly the place to be explicit:
            // a redirect chain is attacker-controlled whenever the document host is.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let url = attempt.url().clone();
                if url.scheme() != "http" && url.scheme() != "https" {
                    return attempt.error(anyhow::anyhow!(
                        "refusing to follow a redirect to a non-http(s) URL: {url}"
                    ));
                }
                if attempt.previous().len() >= 10 {
                    return attempt.error(anyhow::anyhow!("too many redirects"));
                }
                attempt.follow()
            }))
            .build()?;
        Ok(Self {
            client,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Read the body, refusing anything past [`MAX_BODY_BYTES`].
    ///
    /// Enforced while streaming rather than from `Content-Length`: the header is optional under
    /// chunked encoding and is attacker-supplied anyway, so trusting it would cap nothing.
    ///
    /// Every exit below names the source exactly once, which is why the caller wraps this in no
    /// further context (issue #953): a wrap there would re-name the URI on the two branches that
    /// already embed it.
    async fn read_capped(response: reqwest::Response, uri: &str) -> anyhow::Result<String> {
        use futures::StreamExt;

        let mut body: Vec<u8> = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            // Context, not `anyhow!("…: {e}")`: the reqwest error has to stay a link in the chain
            // so `is_timeout()` can still find a body-phase timeout through it (issue #951).
            let chunk = chunk.with_context(|| format!("reading imposter source {uri}"))?;
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                anyhow::bail!(
                    "imposter source {uri} exceeds the {MAX_BODY_BYTES}-byte limit; refusing to \
                     buffer it"
                );
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).with_context(|| format!("imposter source {uri} is not valid UTF-8"))
    }
}

impl ImposterSource for HttpSource {
    fn schemes(&self) -> &'static [&'static str] {
        &["http", "https"]
    }

    fn fetch<'a>(
        &'a self,
        r: &'a SourceRef,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<FetchedImposters>> + Send + 'a>> {
        Box::pin(async move {
            let mut request = self.client.get(&r.uri);
            let previous = self
                .cache
                .lock()
                .map_err(|_| anyhow::anyhow!("imposter source cache poisoned"))?
                .get(&r.uri)
                .cloned();
            if let Some(cached) = &previous {
                request = request.header(reqwest::header::IF_NONE_MATCH, &cached.etag);
            }

            // The reqwest error must stay a link in the chain, not become text: its `Display` is
            // only the kind ("error following redirect", "error sending request"), so the reason —
            // our own `too many redirects` / scheme refusal, or the `TimedOut` marker that tells a
            // timeout from a reset — lives solely in `source()` (issue #951).
            let response = request
                .send()
                .await
                .with_context(|| format!("fetching imposter source {}", r.uri))?;

            // Unchanged: serve the configs parsed on the last fetch. Nothing is re-parsed, and
            // the caller is told so it can skip re-applying an identical config.
            if response.status() == reqwest::StatusCode::NOT_MODIFIED {
                let Some(cached) = previous else {
                    anyhow::bail!(
                        "imposter source {} answered 304 Not Modified without a prior fetch to \
                         reuse",
                        r.uri
                    );
                };
                return Ok(FetchedImposters {
                    configs: cached.configs,
                    // Blocks are boot-only, and a 304 can only happen on a re-fetch, which is
                    // always past boot — so there is nothing a cached block could start.
                    intercept: None,
                    routes: None,
                    meta: SourceMeta {
                        version: Some(cached.etag),
                        fetched_at: SystemTime::now(),
                    },
                    unchanged: true,
                });
            }

            let status = response.status();
            if !status.is_success() {
                anyhow::bail!("imposter source {} returned HTTP {status}", r.uri);
            }

            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = Self::read_capped(response, &r.uri).await?;
            let loaded = config_loader::parse_remote_document(&body, &r.uri)
                .with_context(|| format!("imposter source {}", r.uri))?;

            if let Some(etag) = &etag {
                self.cache
                    .lock()
                    .map_err(|_| anyhow::anyhow!("imposter source cache poisoned"))?
                    .insert(
                        r.uri.clone(),
                        CachedResponse {
                            etag: etag.clone(),
                            configs: loaded.imposters.clone(),
                        },
                    );
            }

            Ok(FetchedImposters {
                configs: loaded.imposters,
                intercept: loaded.intercept,
                routes: loaded.routes,
                meta: SourceMeta {
                    version: etag,
                    fetched_at: SystemTime::now(),
                },
                unchanged: false,
            })
        })
    }
}

/// Scheme → source. Registering a scheme twice is refused at build time rather than resolved by
/// declaration order, so an embedder that shadows a built-in finds out at startup.
#[derive(Default)]
pub struct SourceRegistry {
    by_scheme: HashMap<String, Arc<dyn ImposterSource>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, source: Arc<dyn ImposterSource>) -> anyhow::Result<()> {
        for scheme in source.schemes() {
            if self.by_scheme.contains_key(*scheme) {
                anyhow::bail!(
                    "two imposter sources both claim the `{scheme}:` scheme; each scheme may have \
                     exactly one source"
                );
            }
            self.by_scheme.insert((*scheme).to_string(), source.clone());
        }
        Ok(())
    }

    pub fn get(&self, scheme: &str) -> Option<&Arc<dyn ImposterSource>> {
        self.by_scheme.get(scheme)
    }

    pub fn schemes(&self) -> Vec<&str> {
        let mut schemes: Vec<&str> = self.by_scheme.keys().map(String::as_str).collect();
        schemes.sort_unstable();
        schemes
    }
}

/// The resolved `--imposters` list: the refs, in the order given, and the registry that serves
/// them. Held by the admin server so `POST /admin/reload` re-fetches the same set.
pub struct SourceSet {
    pub refs: Vec<SourceRef>,
    pub registry: SourceRegistry,
}

/// What one round of fetching every source produced.
pub struct MergedSources {
    pub imposters: Vec<ImposterConfig>,
    /// The `intercept` block, from whichever source declared one. Two sources declaring one is
    /// an error, not a silent last-one-wins — there is a single listener to bring up.
    pub intercept: Option<InterceptStartOptions>,
    /// The `routes` block, same one-declarer rule: the front door has one route table.
    pub routes: Option<RouteTable>,
    /// True when *every* source reported no change, so the caller can skip applying a config it
    /// already applied.
    pub all_unchanged: bool,
}

impl SourceSet {
    pub fn new(refs: Vec<SourceRef>, registry: SourceRegistry) -> Self {
        Self { refs, registry }
    }

    /// Fetch every source and merge the results in list order.
    ///
    /// A port declared by two different sources is an error naming both, not a last-one-wins
    /// silent override: the operator wrote two URIs expecting both to be served, and quietly
    /// dropping one of them is the failure mode that is hardest to notice in a running fleet.
    pub async fn fetch_all(&self) -> anyhow::Result<MergedSources> {
        let mut imposters: Vec<ImposterConfig> = Vec::new();
        // port -> the URI that claimed it.
        let mut claimed: HashMap<u16, String> = HashMap::new();
        let mut all_unchanged = true;
        let mut intercept: Option<(String, InterceptStartOptions)> = None;
        let mut routes: Option<(String, RouteTable)> = None;

        for source_ref in &self.refs {
            let scheme = source_ref.scheme();
            let source = self.registry.get(scheme).ok_or_else(|| {
                anyhow::anyhow!(
                    "no imposter source is registered for the `{scheme}:` scheme (from `{}`); \
                     known schemes: {}",
                    source_ref.uri,
                    self.registry.schemes().join(", ")
                )
            })?;

            let fetched = source.fetch(source_ref).await?;
            if !fetched.unchanged {
                all_unchanged = false;
            }

            if let Some(block) = fetched.intercept {
                if let Some((other, _)) = &intercept {
                    anyhow::bail!(
                        "imposter sources `{other}` and `{}` both declare an `intercept` block; \
                         there is one intercept listener, so exactly one source may declare it",
                        source_ref.uri
                    );
                }
                intercept = Some((source_ref.uri.clone(), block));
            }
            if let Some(table) = fetched.routes {
                if let Some((other, _)) = &routes {
                    anyhow::bail!(
                        "imposter sources `{other}` and `{}` both declare a `routes` block; there \
                         is one front-door route table, so exactly one source may declare it",
                        source_ref.uri
                    );
                }
                routes = Some((source_ref.uri.clone(), table));
            }

            for config in fetched.configs {
                // An imposter with no port is auto-assigned at creation, so it cannot collide
                // with anything here.
                if let Some(port) = config.port {
                    if let Some(other) = claimed.get(&port) {
                        anyhow::bail!(
                            "imposter sources `{other}` and `{}` both declare port {port}; each \
                             port may be declared by exactly one source",
                            source_ref.uri
                        );
                    }
                    claimed.insert(port, source_ref.uri.clone());
                }
                imposters.push(config);
            }
        }

        Ok(MergedSources {
            imposters,
            intercept: intercept.map(|(_, block)| block),
            routes: routes.map(|(_, table)| table),
            all_unchanged,
        })
    }
}

/// What `POST /admin/reload` re-reads.
///
/// One slot, not two: a server reloads from the sources it booted from, and making
/// "a datadir *and* a source set" representable would leave the handler picking a winner at
/// runtime for a state the CLI cannot produce.
#[derive(Clone)]
pub enum ReloadSource {
    /// A bare `--datadir`: re-read the directory synchronously, exactly as before U-12.
    Legacy(Arc<ConfigSource>),
    /// `--imposters`, or `--configfile` desugared into one: re-fetch every source.
    Sources(Arc<SourceSet>),
}

/// Split a `--imposters` value into refs. Empty entries are dropped so a trailing comma is not an
/// error.
pub fn parse_uri_list(raw: &str) -> Vec<SourceRef> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(SourceRef::new)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_detection_covers_bare_paths_and_compound_schemes() {
        assert_eq!(SourceRef::new("mocks.json").scheme(), "file");
        assert_eq!(SourceRef::new("./a/b.json").scheme(), "file");
        assert_eq!(SourceRef::new("file:mocks.json").scheme(), "file");
        assert_eq!(SourceRef::new("https://h/i.json").scheme(), "https");
        assert_eq!(SourceRef::new("http://h/i.json").scheme(), "http");
        // The cluster providers (#136) ride the same dispatch; `git+https` must not be read
        // as `git`, or a compound scheme would collide with a plain one.
        assert_eq!(
            SourceRef::new("git+https://h/r#main:p").scheme(),
            "git+https"
        );
        assert_eq!(SourceRef::new("s3://bucket/key").scheme(), "s3");
    }

    #[test]
    fn bare_colon_form_dispatches_on_its_scheme() {
        // The `scheme:rest` spelling is not sugar for a filename — it dispatches, so a cluster
        // provider registered for `s3` is reached by `s3:key` as well as by `s3://bucket/key`.
        assert_eq!(SourceRef::new("s3:bucket/key").scheme(), "s3");
        assert_eq!(SourceRef::new("registry:svc-a,svc-b").scheme(), "registry");
        assert_eq!(SourceRef::new("git+https:h/r#main:p").scheme(), "git+https");
        // Digits, `+`, `-` and `.` are all scheme characters; the first byte must be a letter.
        assert_eq!(SourceRef::new("s3v2:key").scheme(), "s3v2");
        assert_eq!(SourceRef::new("my-source:key").scheme(), "my-source");
        assert_eq!(SourceRef::new("my.source:key").scheme(), "my.source");
    }

    #[test]
    fn a_path_that_merely_contains_a_colon_is_still_a_file() {
        // A Windows drive letter is the case this guard exists for: `C` is scheme-shaped by RFC
        // 3986 but one byte long, so it stays a path and `target()` hands it over verbatim.
        for uri in ["C:\\mocks.json", "C:/mocks.json", "c:\\mocks.json"] {
            assert_eq!(SourceRef::new(uri).scheme(), "file", "{uri}");
            assert_eq!(SourceRef::new(uri).target(), uri, "{uri}");
        }
        // A leading non-letter is not a scheme, however colon-laden the rest is.
        assert_eq!(SourceRef::new("./a:b.json").scheme(), "file");
        assert_eq!(SourceRef::new("/tmp/a:b.json").scheme(), "file");
        assert_eq!(SourceRef::new("../a:b.json").scheme(), "file");
        assert_eq!(SourceRef::new("2024:notes.json").scheme(), "file");
        // An underscore is not in the RFC 3986 scheme set, so this stays a path.
        assert_eq!(SourceRef::new("my_source:key").scheme(), "file");
        // An empty scheme is not a scheme.
        assert_eq!(SourceRef::new(":mocks.json").scheme(), "file");
        // No colon at all, the ordinary case.
        assert_eq!(SourceRef::new("mocks.json").scheme(), "file");
    }

    #[test]
    fn a_scheme_shaped_prefix_wins_even_when_it_was_meant_as_a_path() {
        // The guard is a grammar, not a mind-reader: a path whose leading segment happens to be
        // scheme-shaped now dispatches on it. Both of these fail *loudly* at startup — the
        // registry lookup in `SourceSet::fetch_all` names the scheme and lists the known ones —
        // rather than being opened as a literal filename. `file:` is the escape hatch.
        assert_eq!(SourceRef::new("weird:path.json").scheme(), "weird");
        // A Unix filename may legally end in a colon; such a path must be spelled `file:`.
        assert_eq!(SourceRef::new("mocks.json:").scheme(), "mocks.json");
        assert_eq!(SourceRef::new("file:mocks.json:").scheme(), "file");
        assert_eq!(SourceRef::new("file:mocks.json:").target(), "mocks.json:");
    }

    #[test]
    fn file_prefix_survives_the_grammar_guard() {
        // `file:` used to be recognised by its own arm. It is now just a scheme that satisfies
        // the grammar — same answer, and `target()` still strips exactly that prefix.
        assert_eq!(SourceRef::new("file:mocks.json").scheme(), "file");
        assert_eq!(SourceRef::new("file:mocks.json").target(), "mocks.json");
        assert_eq!(SourceRef::new("file:/abs/mocks.json").scheme(), "file");
        assert_eq!(SourceRef::new("file:C:\\mocks.json").scheme(), "file");
        assert_eq!(
            SourceRef::new("file:C:\\mocks.json").target(),
            "C:\\mocks.json"
        );
    }

    #[test]
    fn target_strips_only_the_file_prefix() {
        assert_eq!(SourceRef::new("file:mocks.json").target(), "mocks.json");
        assert_eq!(SourceRef::new("mocks.json").target(), "mocks.json");
        assert_eq!(
            SourceRef::new("https://h/i.json").target(),
            "https://h/i.json"
        );
    }

    #[test]
    fn uri_list_splits_and_tolerates_blanks() {
        let refs = parse_uri_list("a.json, https://h/b.json ,");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].uri, "a.json");
        assert_eq!(refs[1].uri, "https://h/b.json");
        assert!(parse_uri_list("").is_empty());
    }

    #[test]
    fn registry_refuses_a_duplicate_scheme() {
        let mut registry = SourceRegistry::new();
        registry.register(Arc::new(FileSource::new(false))).unwrap();
        let err = registry
            .register(Arc::new(FileSource::new(false)))
            .expect_err("a second source for `file:` must be refused");
        assert!(
            err.to_string().contains("file"),
            "the error must name the contested scheme: {err}"
        );
    }

    #[test]
    fn registry_reports_known_schemes_for_diagnostics() {
        let mut registry = SourceRegistry::new();
        registry.register(Arc::new(FileSource::new(false))).unwrap();
        registry
            .register(Arc::new(HttpSource::new().unwrap()))
            .unwrap();
        assert_eq!(registry.schemes(), vec!["file", "http", "https"]);
    }
}
