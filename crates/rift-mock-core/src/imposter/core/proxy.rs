//! Proxy record/replay: predicate generation, stub insertion, and upstream proxying.
//!
//! Part of the `Imposter` implementation; see `core/mod.rs` for the struct definition.

use super::*;
use crate::imposter::behavior_pipeline::{BehaviorOutcome, BehaviorRun, ServedParts};
use crate::imposter::types::{UpstreamTarget, upstream_url};
use crate::recording::{ClaimToken, ProxyStoreError, StubPlacement, StubPublication};
// The predicate-generator keys this engine reads (issue #1327). Any other key is accepted and
// reported as unread, never refused: Mountebank configs carry keys Rift may not implement yet.
// Shared with `rift-lint` and the JSON Schema through `rift-types` (issue #1342).
use rift_types::PREDICATE_GENERATOR_KEYS as GENERATOR_KEYS;
use std::hash::BuildHasher;

/// Parts read from a successful upstream proxy response, before recording:
/// `(status, headers, body, latency_ms)`.
type ForwardedResponse = (
    u16,
    Vec<(String, String)>,
    crate::imposter::body_limits::CollectedBody,
    u64,
);

/// The request body as the handler already holds it (issue #1321). `raw` is what the client sent:
/// it is forwarded and keyed on, and is a `Bytes` handle so forwarding it is a refcount bump. A
/// `gzip` / `br` body is forwarded still encoded (issue #1584).
/// `text` is the form the journal and predicates use: the body as-is when it is UTF-8, its base64
/// otherwise (issue #636), after any `Content-Encoding` is decoded. It is `None` for a body that
/// did not decode, which has no body to generate a predicate from (issue #1584). A generated
/// `body` predicate must use `text`, because the matcher compares it against the next request's
/// text form.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProxyBody<'a> {
    pub raw: &'a bytes::Bytes,
    pub text: Option<&'a str>,
}

/// A proxied response ready to serve: the upstream's, or a recorded one replayed.
#[derive(Debug)]
pub(crate) struct ProxiedResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The upstream latency, when `addWaitBehavior` asked for it.
    pub latency_ms: Option<u64>,
}

/// What proxying a request produced.
#[derive(Debug)]
#[must_use]
pub(crate) enum ProxyOutcome {
    Served(ProxiedResponse),
    /// A behavior on the proxy response failed under `strictBehaviors` (#375), a `wait` function
    /// threw, or the imposter was torn down during a `wait` (#1616): serve this instead.
    StrictFailure(hyper::Response<http_body_util::Full<bytes::Bytes>>),
}

/// The upstream response after the proxy response's own behaviors ran on it (issue #1189).
enum Transformed {
    Applied(u16, Vec<(String, String)>, bytes::Bytes),
    /// A behavior failed under the lenient contract: serve these parts, record nothing.
    Degraded(u16, Vec<(String, String)>, bytes::Bytes),
    StrictFailure(hyper::Response<http_body_util::Full<bytes::Bytes>>),
}

/// A won proxy-recording claim (issue #1193). It is released on drop unless settled — which covers
/// the one exit no `return` can: the request future being dropped mid-await, when the client
/// disconnects or the imposter stops. Before, such a claim stayed held for the life of the
/// imposter, and every later identical request was forwarded as `InFlight` and never recorded.
///
/// Releasing from `Drop` is sound because `release_claim` is synchronous and token-checked: a
/// guard whose claim was already re-taken frees nothing.
struct HeldClaim<'a> {
    store: &'a dyn ProxyRecordingStore,
    port: u16,
    signature: &'a RequestSignature,
    /// `Some` until the claim is settled or released.
    token: Option<ClaimToken>,
}

impl<'a> HeldClaim<'a> {
    fn new(
        store: &'a dyn ProxyRecordingStore,
        port: u16,
        signature: &'a RequestSignature,
        token: ClaimToken,
    ) -> Self {
        Self {
            store,
            port,
            signature,
            token: Some(token),
        }
    }

    /// Settle the claim, offering the generated stub when one exists.
    ///
    /// A failed settle releases the claim so the signature stays retryable instead of wedging as
    /// Recorded with nothing behind it (issue #315, and the publication-failure case of #910).
    /// The caller keeps its upstream response either way: the upstream call succeeded, only
    /// recording failed.
    fn settle(mut self, resp: RecordedResponse, publication: Option<&StubPublication<'_>>) {
        let Some(token) = self.token.take() else {
            return;
        };
        // The path is named in the warn below: for a publishing store a `complete` failure is a
        // failed *publication*, which an operator has to correlate differently from a plain
        // recording failure.
        let (path, settled) = match publication {
            Some(publication) => (
                "complete",
                self.store
                    .complete(self.port, self.signature.clone(), token, resp, publication),
            ),
            None => (
                "record",
                self.store
                    .record(self.port, self.signature.clone(), token, resp),
            ),
        };
        if let Err(e) = settled {
            warn!(
                "Failed to settle proxy recording via {path}(), releasing claim so it stays \
                 retryable: {e}"
            );
            self.store.release_claim(self.port, self.signature, token);
        }
    }
}

impl Drop for HeldClaim<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.store.release_claim(self.port, self.signature, token);
        }
    }
}

/// Run `run`'s behaviors on an upstream response. A body that is not UTF-8 goes through as base64
/// and is decoded afterwards, as a binary `is` body is. `content-length` is dropped because the
/// body may have changed length; the server derives it from the body served.
async fn transform_upstream<SH: BuildHasher>(
    run: &BehaviorRun<'_, SH>,
    status: u16,
    headers: Vec<(String, String)>,
    body: bytes::Bytes,
) -> Transformed {
    let (text, binary) = match String::from_utf8(body.to_vec()) {
        Ok(text) => (text, false),
        Err(_) => {
            use base64::Engine;
            (
                base64::engine::general_purpose::STANDARD.encode(&body),
                true,
            )
        }
    };
    let mut grouped: crate::imposter::Headers = crate::imposter::Headers::new();
    for (k, v) in headers {
        grouped.entry(k).or_default().push(v);
    }
    let (parts, mut degraded) = match run
        .apply(ServedParts {
            status,
            // An upstream's status is a number; a script that leaves a string status here is not
            // read, as before (issue #1558 reads one on the `is` and `inject` paths).
            status_text: None,
            // The upstream's body is the author's choice of text (issue #1203): a proxy `lookup`
            // (#1189) exists to expand tokens the upstream returns.
            headers: crate::behaviors::authored_headers(grouped),
            body: crate::behaviors::Spliced::authored(text),
        })
        .await
    {
        BehaviorOutcome::Applied { parts, degraded } => (parts, degraded),
        BehaviorOutcome::StrictFailure(response) => return Transformed::StrictFailure(response),
        // Served in place of the upstream's response, and recorded nowhere (issue #1616).
        BehaviorOutcome::Aborted => {
            return Transformed::StrictFailure(
                crate::imposter::behavior_pipeline::aborted_response(),
            );
        }
    };
    let mut headers: Vec<(String, String)> = parts
        .headers
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("content-length"))
        .flat_map(|(k, values)| {
            let k = k.into_text();
            values.into_iter().map(move |v| (k.clone(), v.into_text()))
        })
        .collect();
    let body = if binary {
        match crate::imposter::handler::decode_binary_body(parts.body.into_text(), run.strict) {
            Ok(crate::imposter::handler::BinaryBody::Decoded(bytes)) => bytes,
            Ok(crate::imposter::handler::BinaryBody::RawFallback(bytes)) => {
                headers.push(("x-rift-binary-error".to_string(), "true".to_string()));
                degraded = true;
                bytes
            }
            Err(strict_failure) => return Transformed::StrictFailure(*strict_failure),
        }
    } else {
        bytes::Bytes::from(parts.body.into_text())
    };
    if degraded {
        Transformed::Degraded(parts.status, headers, body)
    } else {
        Transformed::Applied(parts.status, headers, body)
    }
}

/// The keys of a predicate generator this engine does not read, sorted.
pub(crate) fn unread_generator_keys(generator: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = generator
        .as_object()
        .map(|generator| {
            generator
                .keys()
                .filter(|key| !GENERATOR_KEYS.contains(&key.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    keys.sort_unstable();
    keys
}

/// The generator's `jsonpath` (else `xpath`) body selector, when it is well formed and compiles. A
/// selector that does not compile is not carried: a recorded stub holding it could not be loaded
/// again (#1220). rift-lint reports a malformed shape (W021); a selector that does not compile is
/// only logged here.
fn generator_selector(
    generator: &serde_json::Map<String, serde_json::Value>,
) -> Option<crate::imposter::PredicateSelector> {
    use crate::behaviors::{validate_jsonpath_selector, validate_xpath_selector};
    use crate::imposter::PredicateSelector;

    let (key, value) = generator
        .get_key_value("jsonpath")
        .or_else(|| generator.get_key_value("xpath"))?;
    let selector: PredicateSelector =
        match serde_json::from_value(serde_json::json!({ key: value })) {
            Ok(selector) => selector,
            Err(e) => {
                tracing::warn!(
                    "predicateGenerator `{key}` is malformed ({e}); the body is captured whole"
                );
                return None;
            }
        };
    let compiles = match &selector {
        PredicateSelector::JsonPath { selector } => validate_jsonpath_selector(selector),
        PredicateSelector::XPath { selector, .. } => validate_xpath_selector(selector),
    };
    match compiles {
        Ok(()) => Some(selector),
        Err(e) => {
            tracing::warn!(
                "predicateGenerator `{key}` selector does not compile ({e}); the body is captured whole"
            );
            None
        }
    }
}

/// What `selector` selects from `body`, as Mountebank's generator captures it: one value as a
/// string, several as an array, nothing as `""`.
fn selected_body(selector: &crate::imposter::PredicateSelector, body: &str) -> serde_json::Value {
    use crate::behaviors::{Selection, jsonpath_selection, xpath_selection_in};
    use crate::imposter::PredicateSelector;

    let selection = match selector {
        PredicateSelector::JsonPath { selector } => serde_json::from_str(body)
            .ok()
            .and_then(|json| jsonpath_selection(&json, selector)),
        PredicateSelector::XPath {
            selector,
            namespaces,
        } => match xpath_selection_in(body, selector, namespaces.as_ref()) {
            Ok(selection) => selection,
            // Not yet the failure it is in Mountebank, whose `predicateGenerators` xpath throws
            // too (issue #1585 covers predicates and `copy`): captured as nothing selected, and
            // logged at error level so the recording is not quietly wrong.
            Err(e) => {
                tracing::error!("predicateGenerator: {e}; the body is recorded as \"\"");
                None
            }
        },
    };
    match selection.unwrap_or(Selection::One(String::new())) {
        Selection::One(value) => serde_json::Value::String(value),
        Selection::Many(values) => {
            serde_json::Value::Array(values.into_iter().map(serde_json::Value::String).collect())
        }
    }
}

/// Mountebank's `objFilter`: a string removes that key, an array removes each named key. An object
/// filter removes nothing: Mountebank recurses it into the object-valued fields it names, but every
/// captured value here is a string, so there is nothing to recurse into.
fn remove_ignored(
    captured: &mut serde_json::Map<String, serde_json::Value>,
    filter: &serde_json::Value,
) {
    match filter {
        serde_json::Value::String(key) => {
            captured.remove(key);
        }
        serde_json::Value::Array(keys) => {
            for key in keys.iter().filter_map(serde_json::Value::as_str) {
                captured.remove(key);
            }
        }
        _ => {}
    }
}

/// The client a test recording is made from.
#[cfg(test)]
const TEST_PEER: std::net::SocketAddr =
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 50000);

/// The request a predicate generator captures from.
struct GeneratorRequest<'a, SH> {
    method: &'a str,
    path: &'a str,
    headers: &'a indexmap::IndexMap<String, Vec<String>, SH>,
    body: Option<&'a str>,
    query: Option<&'a str>,
    /// The client's socket, as the matcher reads `requestFrom` and `ip` from it (issue #1446).
    client_addr: std::net::SocketAddr,
}

impl<SH: BuildHasher> GeneratorRequest<'_, SH> {
    /// A header's first value, its name compared case-insensitively. A generated predicate is
    /// single-valued (issue #1025), so the first value of a repeated header is what gets recorded.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .and_then(|(_, values)| values.first())
            .map(String::as_str)
    }

    /// The query as the matcher reads it: a key sent once is recorded as a string and a repeated
    /// key as the array of its values (issue #1553), as Mountebank records it, so the generated
    /// `deepEquals` replays to the request that produced it in any value order.
    fn query(&self) -> serde_json::Map<String, serde_json::Value> {
        self.query
            .map(crate::imposter::parse_query_string)
            .unwrap_or_default()
            .iter()
            .map(|(k, v)| (k.clone(), crate::util::values_as_json(v)))
            .collect()
    }

    /// The urlencoded form, parsed as the matcher parses it, a repeated key as an array like
    /// [`Self::query`]. `None` when the request has none: Mountebank builds `request.form` only for
    /// a non-empty urlencoded body (`httpRequest.js`).
    fn form(&self) -> Option<serde_json::Map<String, serde_json::Value>> {
        crate::imposter::Imposter::request_form(self.headers, self.body).map(|form| {
            form.iter()
                .map(|(k, v)| (k.clone(), crate::util::values_as_json(v)))
                .collect()
        })
    }

    fn headers(&self) -> serde_json::Map<String, serde_json::Value> {
        self.headers
            .iter()
            .filter_map(|(k, values)| {
                values
                    .first()
                    .map(|first| (k.clone(), serde_json::Value::String(first.clone())))
            })
            .collect()
    }
}

/// What a generator's `matches` captured for one field.
enum Captured {
    /// A scalar field (`method`, `path`, `body`, `requestFrom`, `ip`) matched with `true`.
    Scalar(serde_json::Value),
    /// An object field (`query`, `headers`, `form`) matched whole with `true`; `filtered` when the
    /// generator's `ignore` names this field.
    Whole {
        fields: serde_json::Map<String, serde_json::Value>,
        filtered: bool,
    },
    /// An object field matched on the sub-keys the generator lists.
    Listed(serde_json::Map<String, serde_json::Value>),
}

/// Captures `field` as `matcher` asks, or `None` when the matcher asks for nothing.
fn capture<SH: BuildHasher>(
    field: &str,
    matcher: &serde_json::Value,
    request: &GeneratorRequest<'_, SH>,
    selector: Option<&crate::imposter::PredicateSelector>,
    ignore: Option<&serde_json::Value>,
) -> Option<Captured> {
    let whole = matcher.as_bool() == Some(true);
    match field {
        "method" | "path" | "body" if whole => {
            let value = match field {
                "method" => serde_json::Value::String(request.method.to_string()),
                "path" => serde_json::Value::String(request.path.to_string()),
                _ => {
                    let body = request.body.unwrap_or("");
                    match selector {
                        Some(selector) => selected_body(selector, body),
                        None => serde_json::Value::String(body.to_string()),
                    }
                }
            };
            Some(Captured::Scalar(value))
        }
        // The strings the matcher compares (`handler.rs`), so a recording replays to the same
        // client. `requestFrom` carries the client's port, so it matches that connection only, as
        // in Mountebank.
        "requestFrom" if whole => Some(Captured::Scalar(serde_json::Value::String(
            request.client_addr.to_string(),
        ))),
        "ip" if whole => Some(Captured::Scalar(serde_json::Value::String(
            request.client_addr.ip().to_string(),
        ))),
        "query" | "headers" | "form" => {
            // A request without a form records no form predicate: Mountebank records
            // `deepEquals: {form: undefined}`, stored as `deepEquals: {}`, which constrains nothing.
            let form = if field == "form" {
                Some(request.form()?)
            } else {
                None
            };
            let mut fields = if whole {
                match field {
                    "query" => request.query(),
                    "form" => form.unwrap_or_default(),
                    _ => request.headers(),
                }
            } else {
                let listed = matcher.as_object()?;
                let keyed = match field {
                    "query" => Some(request.query()),
                    "form" => form,
                    _ => None,
                };
                // Every listed key, whatever its matcher value, as Mountebank's `buildEquals`
                // records it (issue #1580): `{"q": "mountebank"}` lists `q` as `{"q": true}` does.
                listed
                    .keys()
                    .filter_map(|key| {
                        let value = match &keyed {
                            Some(map) => map.get(key).cloned(),
                            None => request
                                .header(key)
                                .map(|v| serde_json::Value::String(v.to_string())),
                        };
                        value.map(|value| (key.clone(), value))
                    })
                    .collect()
            };
            if let Some(filter) = ignore {
                remove_ignored(&mut fields, filter);
            }
            Some(if whole {
                Captured::Whole {
                    fields,
                    filtered: ignore.is_some(),
                }
            } else {
                Captured::Listed(fields)
            })
        }
        _ => None,
    }
}

/// Mountebank's `predicatesFor` for one `matches` generator: one predicate per matched field.
///
/// With no `predicateOperator`, a field matched whole is recorded with `deepEquals` and one matched
/// on listed sub-keys with `equals` over those keys. Two deviations, both documented in
/// `docs/mountebank/proxy.md`: under an explicit operator Mountebank stores the whole request once
/// per field, Rift stores that field's value; and a whole field the generator's `ignore` filters is
/// recorded with `equals`, since Mountebank does not persist `ignore` and its `deepEquals` recording
/// could never match a request carrying the ignored key.
fn predicates_for_matcher<SH: BuildHasher>(
    generator: &serde_json::Map<String, serde_json::Value>,
    matches: &serde_json::Map<String, serde_json::Value>,
    request: &GeneratorRequest<'_, SH>,
) -> Vec<serde_json::Value> {
    // Checked by `generator_operator_error` before this runs: a string, or absent or `null`.
    let operator = generator.get("predicateOperator").and_then(|p| p.as_str());
    let selector = generator_selector(generator);
    // Copied onto every predicate only when present: an absent `caseSensitive` leaves the
    // predicate default (`false`), as in Mountebank.
    let mut parameters = serde_json::Map::new();
    if let Some(case_sensitive) = generator.get("caseSensitive").filter(|c| c.is_boolean()) {
        parameters.insert("caseSensitive".to_string(), case_sensitive.clone());
    }
    if let Some(except) = generator.get("except").filter(|e| e.is_string()) {
        parameters.insert("except".to_string(), except.clone());
    }

    let mut predicates = Vec::new();
    // Fixed, not the generator's own key order, so two recordings of one request build equal
    // predicate lists — the proxyAlways merge compares them.
    for field in rift_types::GENERATOR_MATCH_FIELDS {
        let Some(matcher) = matches.get(field) else {
            continue;
        };
        let ignore = generator.get("ignore").and_then(|ignore| ignore.get(field));
        let Some(captured) = capture(field, matcher, request, selector.as_ref(), ignore) else {
            continue;
        };
        let (operator, value) = match (operator, captured) {
            (Some("exists"), Captured::Scalar(_)) => ("exists", serde_json::Value::Bool(true)),
            (Some("exists"), Captured::Whole { fields, .. } | Captured::Listed(fields)) => (
                "exists",
                serde_json::Value::Object(
                    fields
                        .into_iter()
                        .map(|(key, _)| (key, serde_json::Value::Bool(true)))
                        .collect(),
                ),
            ),
            (Some(operator), Captured::Scalar(value)) => (operator, value),
            (Some(operator), Captured::Whole { fields, .. } | Captured::Listed(fields)) => {
                (operator, serde_json::Value::Object(fields))
            }
            (None, Captured::Scalar(value)) => ("deepEquals", value),
            (
                None,
                Captured::Whole {
                    fields,
                    filtered: false,
                },
            ) => ("deepEquals", serde_json::Value::Object(fields)),
            (None, Captured::Whole { fields, .. } | Captured::Listed(fields)) => {
                ("equals", serde_json::Value::Object(fields))
            }
        };

        let mut predicate = serde_json::Map::new();
        predicate.insert(
            operator.to_string(),
            serde_json::Value::Object(serde_json::Map::from_iter([(field.to_string(), value)])),
        );
        // The selector scopes the body match the way it scoped the capture. Mountebank copies it
        // onto every predicate, where it would select from the path or method; Rift keeps it on
        // the body's.
        if field == "body"
            && let Some(serde_json::Value::Object(selector)) =
                selector.as_ref().and_then(|s| serde_json::to_value(s).ok())
        {
            predicate.extend(selector);
        }
        predicate.extend(parameters.clone());
        predicates.push(serde_json::Value::Object(predicate));
    }
    predicates
}

/// A generator pass's predicates, as written and as decoded (issue #1353).
///
/// `raw` keys the proxyOnce claim (`RequestSignature::with_predicates` hashes the values as the
/// generator wrote them, so re-serializing would move every claim key); `typed` becomes the
/// recorded stub. Every `raw` element decoded into `typed` and passed the config doors' predicate
/// validation, so recording a stub cannot drop or refuse one.
#[derive(Debug, Default)]
pub(crate) struct GeneratedPredicates {
    raw: Vec<serde_json::Value>,
    typed: Vec<rift_types::Predicate>,
}

/// A failed generator pass, with the `inject` function that was running when it failed (`None`
/// for a failure outside an inject), so the create-time dry run can name it (issue #1580).
#[derive(Debug)]
struct GeneratorFailure {
    inject: Option<String>,
    error: crate::scripting::PredicateGeneratorError,
}

impl From<crate::scripting::PredicateGeneratorError> for GeneratorFailure {
    fn from(error: crate::scripting::PredicateGeneratorError) -> Self {
        Self {
            inject: None,
            error,
        }
    }
}

/// Why the create-time dry run of a stub's `predicateGenerators` refused it (issue #1580).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratorDryRun {
    /// The config's fault: an `inject` threw, returned something other than an array, or ran past
    /// the script timeout. Mountebank answers this with `400 invalid injection`; `source` is the
    /// call it ran (`(<fn>)(config);`) and `data` the error's message, `None` for a thrown value
    /// without one (`throw 'text'`), for which Mountebank leaves `data` out.
    Refused {
        source: String,
        data: Option<String>,
    },
    /// Not the config's fault: the script pool could not run the generator, or its task panicked.
    /// Admitting the config unchecked would skip the check silently, so the door fails instead.
    Unavailable(String),
}

impl std::fmt::Display for GeneratorDryRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused {
                data: Some(data), ..
            } => write!(f, "invalid predicateGenerator injection: {data}"),
            Self::Refused { data: None, .. } => {
                write!(f, "invalid predicateGenerator injection")
            }
            Self::Unavailable(detail) => {
                write!(
                    f,
                    "could not dry-run a predicateGenerator injection: {detail}"
                )
            }
        }
    }
}

// Hand-written rather than `thiserror`, which would read the `source` field as the cause.
impl std::error::Error for GeneratorDryRun {}

/// The peer a dry run's `matches` generators see. The synthetic request has no client: its
/// `requestFrom` is `""` and it has no `ip`, as in Mountebank's dry run.
const DRY_RUN_PEER: std::net::SocketAddr =
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0);

fn has_inject_generator(generators: &[serde_json::Value]) -> bool {
    generators
        .iter()
        .any(|g| g.as_object().is_some_and(|o| o.contains_key("inject")))
}

/// Run every proxy response's `predicateGenerators` once over Mountebank's synthetic dry-run
/// request (an empty `GET /`, with `request.isDryRun` set) and refuse the stubs when an `inject`
/// throws or returns a non-array, as Mountebank's `dryRunValidator` does when an imposter or stub
/// is created (issue #1580). Every proxy mode is checked, `proxyTransparent` included.
///
/// Only the request doors call this, after the `--allowInjection` gate: it runs user code, so it
/// must never run on decode or in `admission_check` (rift-cluster replays stored configs through
/// both on every node), nor on `--datadir` load of configs already admitted. Generator lists
/// without an `inject` cannot fail here and never touch the script pool. A generated predicate
/// the engine would not admit is not refused: Mountebank does not check that at creation, and a
/// recording reports it.
///
/// # Errors
///
/// [`GeneratorDryRun::Refused`] for the config's fault, [`GeneratorDryRun::Unavailable`] when the
/// script pool could not run the check.
pub async fn dry_run_predicate_generators(
    stubs: &[Stub],
    timeout: Duration,
    script_files: Arc<crate::imposter::ScriptFileAccess>,
) -> Result<(), GeneratorDryRun> {
    let lists: Vec<Vec<serde_json::Value>> = stubs
        .iter()
        .flat_map(|stub| &stub.responses)
        .filter_map(|response| match response {
            StubResponse::Proxy { proxy, .. }
                if has_inject_generator(&proxy.predicate_generators) =>
            {
                Some(proxy.predicate_generators.clone())
            }
            _ => None,
        })
        .collect();
    for generators in lists {
        // The first inject names a timeout: the pass cannot say which one was running.
        let first_inject = generators
            .iter()
            .find_map(|g| g.get("inject").and_then(serde_json::Value::as_str))
            .map(str::to_owned);
        let files = Arc::clone(&script_files);
        // Off the async worker, under the script deadline, as a recording runs it (issue #476).
        let handle = tokio::task::spawn_blocking(move || {
            Imposter::run_generators(
                &generators,
                "GET",
                "/",
                &crate::imposter::Headers::new(),
                Some(""),
                None,
                DRY_RUN_PEER,
                &files,
                true,
            )
        });
        let failure = match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(_))) => continue,
            Ok(Ok(Err(failure))) => failure,
            Ok(Err(join_err)) => {
                return Err(GeneratorDryRun::Unavailable(format!(
                    "generator task panicked: {join_err}"
                )));
            }
            Err(_elapsed) => {
                return Err(GeneratorDryRun::Refused {
                    source: injection_source(first_inject.as_deref()),
                    data: Some(format!("timed out after {}ms", timeout.as_millis())),
                });
            }
        };
        use crate::scripting::PredicateGeneratorError as E;
        let data = match failure.error {
            E::Threw { message, .. } => message,
            E::Script(message) | E::Output(message) => Some(message),
            E::Pool(message) => return Err(GeneratorDryRun::Unavailable(message)),
            // Recorded as a failure when a real request generates it; not a create-time refusal.
            E::Invalid(_) => continue,
        };
        return Err(GeneratorDryRun::Refused {
            source: injection_source(failure.inject.as_deref()),
            data,
        });
    }
    Ok(())
}

/// Mountebank's `source` for a generator injection: the call it evaluates.
fn injection_source(inject: Option<&str>) -> String {
    inject.map_or_else(String::new, |f| format!("({f})(config);"))
}

impl Imposter {
    /// The predicates a generator pass writes, for tests that pin their shape. Recording goes
    /// through `generate_stub_predicates`, which keeps the typed form too.
    #[cfg(test)]
    pub(crate) fn generate_predicates_from_request<SH: BuildHasher>(
        &self,
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &indexmap::IndexMap<String, Vec<String>, SH>,
        body: Option<&str>,
        query: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, crate::scripting::PredicateGeneratorError> {
        Self::generate_predicates_impl(
            generators,
            method,
            path,
            headers,
            body,
            query,
            TEST_PEER,
            &self.script_file_access(),
        )
        .map(|generated| generated.raw)
    }

    /// [`Self::generate_predicates_from_request`] without `&self`, so the proxy-recording path
    /// can run it on `spawn_blocking` (issue #476) — a `predicateGenerators.inject` script must
    /// not execute (and block on the MB script pool) on a tokio async worker. `script_files` is what
    /// a `predicateGenerators.inject` may `require()` (issue #1611).
    #[allow(clippy::too_many_arguments)]
    fn generate_predicates_impl<SH: BuildHasher>(
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &indexmap::IndexMap<String, Vec<String>, SH>,
        body: Option<&str>,
        query: Option<&str>,
        client_addr: std::net::SocketAddr,
        script_files: &std::sync::Arc<crate::imposter::ScriptFileAccess>,
    ) -> Result<GeneratedPredicates, crate::scripting::PredicateGeneratorError> {
        Self::run_generators(
            generators,
            method,
            path,
            headers,
            body,
            query,
            client_addr,
            script_files,
            false,
        )
        .map_err(|failure| failure.error)
    }

    /// The generator pass itself. `dry_run` is the create-time check (issue #1580): an inject then
    /// sees Mountebank's synthetic request — `requestFrom: ""`, no `ip`, an empty `form` and
    /// `isDryRun: true`.
    #[allow(clippy::too_many_arguments)]
    fn run_generators<SH: BuildHasher>(
        generators: &[serde_json::Value],
        method: &str,
        path: &str,
        headers: &indexmap::IndexMap<String, Vec<String>, SH>,
        body: Option<&str>,
        query: Option<&str>,
        client_addr: std::net::SocketAddr,
        script_files: &std::sync::Arc<crate::imposter::ScriptFileAccess>,
        dry_run: bool,
    ) -> Result<GeneratedPredicates, GeneratorFailure> {
        let mut predicates = Vec::new();
        let invalid = crate::scripting::PredicateGeneratorError::Invalid;

        for (i, r#gen) in generators.iter().enumerate() {
            // A replayed config skipped the config doors' check of the same shapes (#1413): an
            // entry the recorder cannot read fails the generation instead of being skipped, which
            // recorded fewer predicates than asked for, down to a match-all stub.
            if let Some(refusal) = rift_types::generator_shape_error(i, r#gen)
                .or_else(|| crate::imposter::types::generator_operator_error(i, r#gen))
            {
                return Err(invalid(refusal).into());
            }
            let Some(gen_obj) = r#gen.as_object() else {
                return Err(invalid(format!("predicateGenerators[{i}] must be an object")).into());
            };

            // Handle inject predicateGenerator — calls a JS function with the request and
            // predicates built so far; the function returns additional predicate objects.
            if let Some(inject_fn) = gen_obj.get("inject").and_then(|v| v.as_str()) {
                #[cfg(feature = "javascript")]
                {
                    use crate::scripting::{MountebankRequest, execute_predicate_generator_inject};
                    let query_map = query
                        .map(crate::imposter::parse_query_string)
                        .unwrap_or_default();
                    let mb_request = MountebankRequest {
                        method: method.to_string(),
                        path: path.to_string(),
                        // A script sees a repeated key comma-joined (`docs/mountebank/responses.md`).
                        query: crate::util::comma_joined(&query_map),
                        // `MountebankRequest.headers` is the fixed single-value scripting boundary
                        // (out of scope for #704/#1025), so a repeated header exposes only its
                        // first value to a predicateGenerator inject script.
                        headers: headers
                            .iter()
                            .filter_map(|(k, v)| v.first().map(|first| (k.clone(), first.clone())))
                            .collect(),
                        // `body` is already the classified string from the caller (base64 for a
                        // binary request body, issue #636); this path doesn't thread the mode
                        // flag through separately, so default to `Text`.
                        body: body.map(|b| b.to_string()),
                        // A body that did not decode reaches a generator as a missing one (issue
                        // #1584), so this script reads it as `""`.
                        body_undefined: false,
                        mode: None,
                        // Mountebank's dry-run request has an empty `requestFrom`, no `ip` and an
                        // empty `form` (`baseHttpServer.js`).
                        request_from: Some(if dry_run {
                            String::new()
                        } else {
                            client_addr.to_string()
                        }),
                        ip: (!dry_run).then(|| client_addr.ip().to_string()),
                        form: if dry_run {
                            Some(crate::util::OrderedMap::default())
                        } else {
                            crate::imposter::Imposter::request_form(headers, body)
                                .map(|form| crate::util::comma_joined(&form))
                        },
                        is_dry_run: dry_run,
                    };
                    let inject_preds = execute_predicate_generator_inject(
                        inject_fn,
                        &mb_request,
                        &predicates,
                        script_files,
                    )
                    .map_err(|error| GeneratorFailure {
                        inject: Some(inject_fn.to_owned()),
                        error,
                    })?;
                    predicates.extend(inject_preds);
                    continue;
                }
                // Ignoring it would record the remaining predicates, or none: a match-all stub
                // (issues #498, #1353). Without a script engine the generation fails instead.
                #[cfg(not(feature = "javascript"))]
                {
                    let _ = (script_files, dry_run);
                    return Err(GeneratorFailure {
                        inject: Some(inject_fn.to_owned()),
                        error: crate::scripting::PredicateGeneratorError::Script(
                            "predicateGenerator inject requires the 'javascript' feature"
                                .to_string(),
                        ),
                    });
                }
            }

            let Some(matches) = gen_obj.get("matches").and_then(|m| m.as_object()) else {
                return Err(invalid(format!(
                    "predicateGenerators[{i}] needs `matches` or `inject`"
                ))
                .into());
            };
            let request = GeneratorRequest {
                method,
                path,
                headers,
                body,
                query,
                client_addr,
            };
            predicates.extend(predicates_for_matcher(gen_obj, matches, &request));
        }

        // Every generator kind ends here, so an inject typo, an operator that cannot hold a field
        // map, or a `matches` value that is no regex fails the generation (#498) instead of being
        // dropped from the stub or recorded as a stub no config door admits (issue #1353).
        let typed = predicates
            .iter()
            .map(|value| {
                serde_json::from_value::<rift_types::Predicate>(value.clone())
                    .map_err(|e| invalid(format!("{value}: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        crate::imposter::types::validate_predicates(&typed).map_err(invalid)?;
        Ok(GeneratedPredicates {
            raw: predicates,
            typed,
        })
    }

    /// Resolve a proxy mode string to the placement its recorded stubs take.
    ///
    /// The single source of truth for that mapping: the engine's own insertion and the
    /// [`StubPublication`] handed to a publishing store both go through here, so the position a
    /// publisher is told to reproduce can never drift from the one the engine would use.
    fn placement_for_mode(proxy_mode: &str) -> StubPlacement {
        // Parsed like every other reader of the mode (issue #1314): `proxyalways` used to get a
        // proxyAlways store but proxyOnce placement.
        if ProxyMode::parse(proxy_mode).unwrap_or_default() == ProxyMode::ProxyAlways {
            StubPlacement::AfterProxyMerging
        } else {
            StubPlacement::BeforeProxy
        }
    }

    /// Place a recorded stub next to the proxy stub that answered (issue #1389), as Mountebank
    /// places it by `stubIndex()`:
    ///
    /// - `proxyOnce`: in front of that stub, so the recording matches first next time;
    /// - `proxyAlways`: merged into the first stub after it with equal predicates, else at the end
    ///   of the list.
    ///
    /// `answering` is the answering stub's slot, re-located under the write lock so an index made
    /// stale by a concurrent edit cannot misplace the recording. When it is `None` or that stub is
    /// gone (deleted while the request was in flight), the anchor is the first stub proxying to
    /// `proxy_to`. With neither, a `proxyOnce` recording goes last and a `proxyAlways` one merges
    /// from the top of the list.
    ///
    /// `claim` is the `proxyOnce` signature the recording settled under: the new stub carries it,
    /// so a later edit that removes the stub forgets the claim too (issue #1386).
    pub fn insert_or_append_proxy_stub(
        &self,
        stub: Stub,
        proxy_to: &str,
        proxy_mode: &str,
        answering: Option<u64>,
        claim: Option<RequestSignature>,
    ) {
        let placement = Self::placement_for_mode(proxy_mode);
        self.mutate_stubs(|stubs| {
            let anchor = answering
                .and_then(|slot| stubs.iter().position(|s| s.slot == slot))
                .or_else(|| stubs.iter().position(|s| proxies_to(&s.stub, proxy_to)));

            if placement == StubPlacement::AfterProxyMerging {
                // Structural comparison, not serialized JSON (issue #611): a predicate's operands
                // are `HashMap`s, which serialize in iteration order. An empty list merges too, as
                // in Mountebank: a recording made without generators has no predicates.
                let from = anchor.map_or(0, |a| a + 1);
                let matching = stubs
                    .iter()
                    .skip(from)
                    .position(|existing| existing.stub.predicates == stub.predicates)
                    .map(|p| p + from);

                if let Some(idx) = matching {
                    // Append responses to the existing stub. States live behind `Arc` (issue #287),
                    // so rebuild the entry with the extended responses while reusing the slot's
                    // cycler + slot token. The append joins the tail of the stub's current cycle,
                    // as in Mountebank's rotating queue (issue #1513).
                    let merged = self.appended(&stubs[idx], stub.responses);
                    let total = merged.stub.responses.len();
                    // Recording stays on the wall clock, not the load clock (issue #1301): it builds
                    // a state per proxied request, and one stamp each would push the clock a second
                    // ahead per request. But never stamp earlier than the stub already served — an
                    // admin change may have left that stamp ahead of the wall clock.
                    let at = stubs[idx].loaded_at().max(chrono::Utc::now());
                    stubs[idx] = Arc::new(merged.with_loaded_at(at));
                    debug!(
                        "Appended response to existing stub at index {idx} (proxyAlways mode, {total} total responses)"
                    );
                    return;
                }

                stubs.push(Arc::new(StubState::new(stub)));
                debug!(
                    "Appended generated stub at index {} (proxyAlways mode)",
                    stubs.len() - 1
                );
            } else {
                let index = anchor.unwrap_or(stubs.len());
                let mut state = StubState::new(stub);
                state.recorded_claim = claim.map(Arc::new);
                stubs.insert(index, Arc::new(state));
                debug!(
                    "Inserted generated stub at index {} before proxy (proxyOnce mode)",
                    index
                );
            }
        });
    }

    /// Run `predicateGenerators` over a request: the predicates of the stub its response is
    /// recorded as.
    ///
    /// `Err((token, detail))` = generation failed and the predicates are unknown. The caller must
    /// then record no stub: an empty or partial predicate list matches every future request (issue
    /// #498). `token` is a short, header-safe category surfaced to the client; `detail` is logged.
    async fn generate_stub_predicates<SH>(
        &self,
        proxy_config: &ProxyResponse,
        method: &str,
        uri: &hyper::Uri,
        headers: &indexmap::IndexMap<String, Vec<String>, SH>,
        body: Option<ProxyBody<'_>>,
        client_addr: std::net::SocketAddr,
    ) -> Result<GeneratedPredicates, (&'static str, String)>
    where
        SH: BuildHasher + Clone + Send + 'static,
    {
        // An `inject` generator executes a JS script; run the generator pass off the async worker
        // under the script deadline (issue #476). Script-free generator lists (the common case)
        // keep the inline path — pure predicate building, no script pool.
        if !has_inject_generator(&proxy_config.predicate_generators) {
            return Self::generate_predicates_impl(
                &proxy_config.predicate_generators,
                method,
                uri.path(),
                headers,
                body.and_then(|b| b.text),
                uri.query(),
                client_addr,
                &self.script_file_access(),
            )
            .map_err(|e| (e.kind(), e.to_string()));
        }
        let generators = proxy_config.predicate_generators.clone();
        let method = method.to_string();
        let path = uri.path().to_string();
        let headers = headers.clone();
        let body = body.and_then(|b| b.text.map(str::to_string));
        let query = uri.query().map(str::to_string);
        let script_files = self.script_file_access();
        let timeout = std::time::Duration::from_millis(
            crate::scripting::resolve_script_timeout_ms(&self.config),
        );
        // Plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987):
        // `execute_predicate_generator_inject` never installs a flow store, so a predicate
        // generator has no `ctx.state` and nothing here can annotate.
        let handle = tokio::task::spawn_blocking(move || {
            Self::generate_predicates_impl(
                &generators,
                &method,
                &path,
                &headers,
                body.as_deref(),
                query.as_deref(),
                client_addr,
                &script_files,
            )
        });
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(preds))) => Ok(preds),
            Ok(Ok(Err(gen_err))) => Err((gen_err.kind(), gen_err.to_string())),
            Ok(Err(join_err)) => {
                Err(("task-panic", format!("generator task panicked: {join_err}")))
            }
            Err(_elapsed) => Err((
                "timeout",
                format!("timed out after {}ms", timeout.as_millis()),
            )),
        }
    }

    /// Forward a request through proxy and optionally record the response.
    ///
    /// `behaviors` are the proxy response's own behaviors. They run on the upstream response before
    /// anything is recorded, so the client, the recording and the generated stub all get the
    /// transformed response, as in Mountebank's `proxyAndRecord` (issue #1189). A replay of a
    /// recording runs none of them — it was recorded transformed. A behavior that failed records
    /// nothing.
    // The request's parts are borrowed from the handler's own locals; bundling them would only
    // move the same eight borrows into a struct built at the two call sites.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_proxy_request<SH>(
        &self,
        proxy_config: &ProxyResponse,
        // A stub's `to` is an origin; `defaultForward` is a base URL (issue #1550).
        target: UpstreamTarget,
        method: &str,
        uri: &hyper::Uri,
        headers: &indexmap::IndexMap<String, Vec<String>, SH>,
        body: Option<ProxyBody<'_>>,
        behaviors: Option<&BehaviorRun<'_, SH>>,
        answering: Option<u64>,
        // The client, for a generator that records `requestFrom` or `ip` (issue #1446).
        client_addr: std::net::SocketAddr,
    ) -> anyhow::Result<ProxyOutcome>
    where
        // `Clone + Send + 'static`: an inject predicateGenerator clones `headers` into a
        // `spawn_blocking` `'static` closure below.
        SH: BuildHasher + Clone + Send + 'static,
    {
        let client = super::resolve_upstream_client(self.upstream_client.as_ref())?;
        let client = &*client;

        info!(
            "Proxy config - addDecorateBehavior: {:?}, addWaitBehavior: {}, predicateGenerators: {:?}",
            proxy_config.add_decorate_behavior,
            proxy_config.add_wait_behavior,
            proxy_config.predicate_generators
        );

        // Build the proxy URL, applying path rewrite if configured
        let original_path = uri.path();
        let rewritten_path = if let Some(ref rewrite) = proxy_config.path_rewrite {
            original_path.replacen(&rewrite.from, &rewrite.to, 1)
        } else {
            original_path.to_string()
        };

        // Before the claim, so a `to` that does not parse fails the request with nothing to release.
        let target_url = upstream_url(&proxy_config.to, target, &rewritten_path, uri.query())
            .map_err(anyhow::Error::msg)?
            .to_string();

        if proxy_config.path_rewrite.is_some() {
            debug!(
                "Proxy request to: {} (path rewritten from '{}')",
                target_url, original_path
            );
        } else {
            debug!("Proxy request to: {}", target_url);
        }

        // An unknown mode is admitted only from a replayed config; it runs as the default.
        let mode = proxy_config.mode().unwrap_or_default();
        let transparent = mode == ProxyMode::ProxyTransparent;
        // Only a proxyOnce claim is ever replayed, so only it ties a recorded stub to the store.
        let mode_is_once = mode == ProxyMode::ProxyOnce;
        // A proxied response is recorded as a stub when predicateGenerators, addWaitBehavior or
        // addDecorateBehavior is configured, as in Mountebank — never for `proxyTransparent`,
        // which forwards every request and records nothing.
        let generates_stub = !transparent
            && (!proxy_config.predicate_generators.is_empty()
                || proxy_config.add_wait_behavior
                || proxy_config.add_decorate_behavior.is_some());
        // The predicates come from the request alone, so they are generated before the claim:
        // with generators they are the request's identity, and the claim keys on them.
        let generation = if !generates_stub {
            None
        } else if proxy_config.predicate_generators.is_empty() {
            // No generators: the stub has no predicates and matches every request.
            Some(Ok(GeneratedPredicates::default()))
        } else {
            Some(
                self.generate_stub_predicates(
                    proxy_config,
                    method,
                    uri,
                    headers,
                    body,
                    client_addr,
                )
                .await,
            )
        };

        // The replay key. Without predicateGenerators nothing names the request's identity, so it
        // is method, path, query and body (issue #1317); headers stay out. With generators the
        // identity is the generated predicates (issue #1333): proxyOnce inserts the recorded stub
        // before the proxy stub, so a request reaching the proxy stub missed every recording, and
        // keying it on method, path and query alone answered it with another identity's recording
        // instead of forwarding it — Mountebank's `proxyAndRecord` forwards and records on every
        // miss.
        let mut signature = RequestSignature::new(method, uri.path(), uri.query(), &[]);
        if proxy_config.predicate_generators.is_empty() {
            signature = signature.with_body(body.map_or(&[][..], |b| b.raw.as_ref()));
        } else if let Some(Ok(predicates)) = &generation {
            signature = signature.with_predicates(&predicates.raw);
        }
        let port = self.journal_port();

        // Consult the proxy-recording gate. `AlreadyRecorded` replays; `Claimed` grants the
        // right to record; `InFlight` (a concurrent proxyOnce loser) and an unavailable
        // store proxy upstream without recording.
        // The claim carries this response's own mode (issue #1387), so a `proxyAlways` stub never
        // replays what a `proxyOnce` one recorded. A `proxyTransparent` response — or a
        // `defaultForward` — records nothing and takes no claim at all (issue #1314).
        // A failed generation has no identity to key on, records no stub (issue #498) and must not
        // replay either: under the method, path and query key it would answer with another
        // request's recording, without the `x-rift-generator-error` the client should see.
        let generation_failed = matches!(generation, Some(Err(_)));
        // A concurrent proxyOnce loser forwards, but the winner records the identity: inserting
        // its stub too would stack a duplicate per loser.
        let mut lost_race = false;
        let claim = if transparent || generation_failed {
            None
        } else {
            match self.proxy_store.try_claim_as(port, &signature, mode) {
                Ok(ClaimOutcome::AlreadyRecorded) => {
                    if let Some(recorded) = self.proxy_store.lookup(port, &signature) {
                        debug!("Returning recorded proxy response (proxyOnce mode)");
                        return Ok(ProxyOutcome::Served(ProxiedResponse {
                            status: recorded.status,
                            headers: recorded.headers,
                            body: recorded.body,
                            latency_ms: recorded.latency_ms,
                        }));
                    }
                    // AlreadyRecorded but nothing to replay: a race (a concurrent clear, or the
                    // recording forgotten because its stub was just deleted, #1386) or a
                    // misbehaving backend. Forward without recording rather than fail; the next
                    // request claims and records it.
                    warn!(
                        "Proxy store reported AlreadyRecorded but lookup found nothing; forwarding without recording"
                    );
                    None
                }
                Ok(ClaimOutcome::InFlight) => {
                    lost_race = true;
                    None
                }
                Ok(ClaimOutcome::Claimed(token)) => Some(HeldClaim::new(
                    self.proxy_store.as_ref(),
                    port,
                    &signature,
                    token,
                )),
                // The store arbitrates exactly-once and could not answer: fail the request rather
                // than forward it. Forwarding here would call the upstream while *nothing* is
                // serializing claims, so the duplicate is bounded by the outage, not by one racing
                // window — the outcome `proxyOnce` exists to prevent. The `BackendUnavailable` rides
                // the chain so the response boundary can answer 503 through the #318 door.
                Err(ProxyStoreError::Refused(cause)) => {
                    return Err(anyhow::Error::new(cause)
                        .context("proxyOnce claim refused; request not forwarded"));
                }
                Err(e) => {
                    warn!("Proxy recording store unavailable; forwarding without recording: {e}");
                    None
                }
            }
        };

        // Forward the request. Isolated so a failure returns early, dropping (releasing) the claim
        // (issue #315): a proxyOnce signature must stay retryable, not wedge because the upstream
        // call errored.
        let start = Instant::now();
        let forwarded: anyhow::Result<ForwardedResponse> = async {
            // Forward the method verbatim, as Mountebank does (issue #1380).
            let forwarded_method = reqwest::Method::from_bytes(method.as_bytes())
                .with_context(|| format!("invalid request method `{method}`"))?;
            let mut request = client.request(forwarded_method, &target_url);

            // Copy headers (excluding host). One `.header()` call PER VALUE, in send order
            // (issue #1025) — reqwest appends rather than replacing on a repeated call, so a
            // repeated header is forwarded in full instead of collapsing to whichever value used
            // to survive the old single-value map.
            for (key, values) in headers {
                let key_lower = key.to_lowercase();
                if key_lower != "host" && key_lower != "content-length" {
                    for value in values {
                        request = request.header(key, value);
                    }
                }
            }

            // Add inject headers
            for (key, value) in proxy_config.inject_headers.iter() {
                request = request.header(key, value);
            }

            // Add body if present
            if let Some(proxy_body) = body {
                request = request.body(proxy_body.raw.clone());
            }

            // Send request
            let response = request
                .send()
                .await
                .with_context(|| format!("Failed to send proxy request to {target_url}"))?;
            let latency_ms = start.elapsed().as_millis() as u64;

            let status = response.status().as_u16();
            // Issue #999: the upstream hop, observed off the timing the `addWaitBehavior` path
            // already captures — so this measures the forward itself, not rift's own handling.
            crate::extensions::metrics::record_upstream_duration(method, status, latency_ms as f64);
            // This one list feeds three destinations — the client response, the
            // `RecordedResponse`, and the stub `proxyOnce`/`proxyAlways` generates — so a value
            // mangled here is mangled in all three, and the stub persists it (issue #1041).
            let response_headers =
                crate::imposter::headers::collect_response_headers(response.headers());
            // Read under the shared body limits (issue #1439): a known length over the ceiling is
            // refused before a byte is read, and the body holds its share of the in-flight budget
            // until this request's response is built. It never waits for the budget: the request's
            // own body already holds a share, and proxied requests waiting on each other would all
            // time out. Either refusal fails the proxy as a 502, like any upstream failure.
            let mut response = response;
            let mut collector = self
                .body_guard
                .start_without_waiting(response.content_length())
                .await
                .map_err(|e| anyhow::anyhow!("Proxy response body from {target_url}: {e}"))?;
            while let Some(chunk) = response
                .chunk()
                .await
                .with_context(|| format!("Failed to read response body from {target_url}"))?
            {
                collector
                    .push(&chunk)
                    .map_err(|e| anyhow::anyhow!("Proxy response body from {target_url}: {e}"))?;
            }
            let body_bytes = collector.finish();

            Ok((status, response_headers, body_bytes, latency_ms))
        }
        .await;

        let (status, response_headers, upstream_body, latency_ms) = match forwarded {
            Ok(parts) => parts,
            // Returning drops `claim`, which releases it: the signature stays retryable (#315).
            Err(e) => return Err(e),
        };
        // `upstream_body` keeps its share of the body budget until this returns (issue #1439).
        let body_bytes = upstream_body.bytes().clone();
        let recorded_latency = proxy_config.add_wait_behavior.then_some(latency_ms);

        // After the forward, so a `wait` never counts toward the latency `addWaitBehavior` records;
        // before the recording, so what is recorded is what the client got.
        let (status, mut response_headers, body_bytes) = match behaviors {
            None => (status, response_headers, body_bytes),
            Some(run) => {
                match transform_upstream(run, status, response_headers, body_bytes).await {
                    Transformed::Applied(status, headers, body) => (status, headers, body),
                    // A failed behavior records nothing; returning drops (releases) the claim.
                    Transformed::Degraded(status, headers, body) => {
                        return Ok(ProxyOutcome::Served(ProxiedResponse {
                            status,
                            headers,
                            body: body.to_vec(),
                            latency_ms: recorded_latency,
                        }));
                    }
                    Transformed::StrictFailure(response) => {
                        return Ok(ProxyOutcome::StrictFailure(response));
                    }
                }
            }
        };

        // Build the recording if we hold a claim, but do NOT settle the claim yet: a store that
        // publishes stubs must be able to make "Recorded" conditional on publishing the stub, and
        // the stub does not exist until predicate generation below has run (issue #910).
        let mut recording = claim.map(|claim| {
            (
                claim,
                RecordedResponse {
                    status,
                    headers: response_headers.clone(),
                    body: body_bytes.to_vec(),
                    latency_ms: recorded_latency,
                    timestamp_secs: crate::util::unix_timestamp(),
                },
            )
        });

        if let Some(generation) = generation {
            match generation {
                Ok(predicates) => {
                    // `addDecorateBehavior` is written into the SAVED stub's behaviors and not
                    // applied to this live response, as in Mountebank; it runs when the saved
                    // stub replays. The proxy response's own behaviors already ran above, and the
                    // stub holds their result — never the behaviors themselves, so nothing runs
                    // twice (Mountebank's `newIsResponse`).
                    let new_stub = create_stub_from_proxy_response(
                        predicates.typed,
                        status,
                        &response_headers,
                        &body_bytes,
                        recorded_latency,
                        proxy_config.add_decorate_behavior.clone(),
                        Some(proxy_config.to.clone()),
                        latency_ms,
                    );

                    // Insert or append the stub based on proxy mode
                    // proxyOnce: Insert new stub before the proxy stub
                    // proxyAlways: Append response to existing stub with matching predicates
                    // `placement_for_mode` parses it, so the string is passed as written.
                    let mode = proxy_config.mode.as_str();

                    // Settle the claim now that the stub exists, and before it is published, so a
                    // publishing store can refuse to commit "Recorded" if publication fails.
                    let settled = if let Some((claim, resp)) = recording.take() {
                        let publication = StubPublication {
                            stub: &new_stub,
                            placement: Self::placement_for_mode(mode),
                            proxy_to: &proxy_config.to,
                            anchor_index: answering.and_then(|slot| {
                                self.snapshot().stubs().iter().position(|s| s.slot == slot)
                            }),
                        };
                        claim.settle(resp, Some(&publication));
                        true
                    } else {
                        false
                    };

                    // A store that publishes stubs is the only publisher; inserting locally too
                    // would double-publish the recording (issue #910). When no claim was won there
                    // was no `complete` call either, so nobody publishes this stub — say so, rather
                    // than logging that the store took ownership of something it never saw.
                    if self.proxy_store.publishes_stubs() {
                        debug!(
                            "Skipping local stub insertion for path {} (proxy store publishes \
                             stubs; handed over: {settled})",
                            uri.path()
                        );
                    } else if lost_race {
                        debug!(
                            "Skipping local stub insertion for path {} (a concurrent request holds \
                             the claim and records it)",
                            uri.path()
                        );
                    } else {
                        self.insert_or_append_proxy_stub(
                            new_stub,
                            &proxy_config.to,
                            mode,
                            answering,
                            (settled && mode_is_once).then(|| signature.clone()),
                        );
                    }
                    debug!(
                        "Generated stub from proxy response for path {} (mode: {})",
                        uri.path(),
                        mode
                    );
                }
                Err((token, detail)) => {
                    // Predicate generation failed — record nothing rather than a match-all stub,
                    // and mark the proxied response so the failure is client-visible, not a
                    // server-only warn (issue #498).
                    warn!(
                        "predicate generation failed for path {} ({detail}); skipping auto-stub to \
                         avoid recording a match-all stub",
                        uri.path()
                    );
                    response_headers
                        .push(("x-rift-generator-error".to_string(), token.to_string()));
                }
            }
        }

        // Nothing is configured to generate a stub (a failed generation took no claim), so there
        // is nothing to publish and the claim settles through `record` exactly as before.
        if let Some((claim, resp)) = recording {
            claim.settle(resp, None);
        }

        Ok(ProxyOutcome::Served(ProxiedResponse {
            status,
            headers: response_headers,
            body: body_bytes.to_vec(),
            latency_ms: recorded_latency,
        }))
    }
}

/// Whether `stub` has a response proxying to `to`.
fn proxies_to(stub: &Stub, to: &str) -> bool {
    stub.responses
        .iter()
        .any(|r| matches!(r, StubResponse::Proxy { proxy, .. } if proxy.to == to))
}

#[cfg(test)]
mod proxy_dedup_tests {
    use super::*;
    use serde_json::json;

    fn imposter_with_proxy(to: &str) -> Imposter {
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": { "to": to, "mode": "proxyAlways" } }] }],
        }))
        .expect("valid imposter config");
        Imposter::new(cfg).expect("test imposter")
    }

    /// A stub whose single predicate matches on several fields at once, as a hand-written stub or a
    /// recording from before #1329 may (a generator now records one predicate per field).
    fn multi_key_stub(body: &str) -> Stub {
        serde_json::from_value(json!({
            "predicates": [{ "equals": { "method": "GET", "path": "/x", "query": { "a": "1" } } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }],
        }))
        .expect("valid stub")
    }

    // Issue #611: dedup compared *serialized* predicates, but a predicate's operands are `HashMap`s
    // that serialize in iteration order — so two semantically equal multi-key predicate sets
    // produced different strings and proxyAlways appended a duplicate stub instead of merging the
    // recorded response into the existing one.
    #[test]
    fn proxy_always_merges_responses_for_equal_multi_key_predicates() {
        let imposter = imposter_with_proxy("http://upstream");

        imposter.insert_or_append_proxy_stub(
            multi_key_stub("first"),
            "http://upstream",
            "proxyAlways",
            None,
            None,
        );
        imposter.insert_or_append_proxy_stub(
            multi_key_stub("second"),
            "http://upstream",
            "proxyAlways",
            None,
            None,
        );

        let stubs = imposter.get_stubs();
        assert_eq!(
            stubs.len(),
            2,
            "equal predicate sets must merge into one recorded stub alongside the proxy stub, \
             not append a duplicate"
        );
        assert_eq!(
            stubs[1].responses.len(),
            2,
            "both recorded responses must land on the single matching stub"
        );
    }
}

/// Issue #1193: a won claim is released when the request future is dropped, and only then.
#[cfg(test)]
mod held_claim_tests {
    use super::*;
    use crate::recording::{LocalProxyStore, ProxyMode, ProxyRecordingStore};

    fn signature() -> RequestSignature {
        RequestSignature::new("GET", "/p", None, &[])
    }

    fn recorded() -> RecordedResponse {
        RecordedResponse {
            status: 200,
            headers: vec![],
            body: b"up".to_vec(),
            latency_ms: None,
            timestamp_secs: 0,
        }
    }

    fn claim(store: &LocalProxyStore, sig: &RequestSignature) -> ClaimToken {
        match store.try_claim(1, sig).expect("store answers") {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    #[test]
    fn dropping_a_held_claim_releases_it() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        let held = HeldClaim::new(&store, 1, &sig, claim(&store, &sig));
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::InFlight
        );
        drop(held);
        assert!(
            matches!(
                store.try_claim(1, &sig).expect("answers"),
                ClaimOutcome::Claimed(_)
            ),
            "a dropped claim must be free to take again"
        );
    }

    #[test]
    fn settling_a_held_claim_records_it_and_releases_nothing() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        HeldClaim::new(&store, 1, &sig, claim(&store, &sig)).settle(recorded(), None);
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::AlreadyRecorded
        );
        assert_eq!(store.lookup(1, &sig).map(|r| r.body), Some(b"up".to_vec()));
    }

    /// The store ignores a stale token, so a guard dropped after its claim was re-taken (e.g. by a
    /// `clear`) cannot free the new holder.
    #[test]
    fn a_stale_guard_does_not_release_a_newer_claim() {
        let store = LocalProxyStore::new(ProxyMode::ProxyOnce);
        let sig = signature();
        let stale = claim(&store, &sig);
        store.release_claim(1, &sig, stale);
        let _current = claim(&store, &sig);
        drop(HeldClaim::new(&store, 1, &sig, stale));
        assert_eq!(
            store.try_claim(1, &sig).expect("answers"),
            ClaimOutcome::InFlight
        );
    }
}

/// Issue #1327: Mountebank's `jsonpath`, `xpath` and `ignore` predicate-generator keys.
#[cfg(test)]
mod predicate_generator_selector_tests {
    use super::*;
    use serde_json::json;

    fn generate(
        generator: serde_json::Value,
        body: Option<&str>,
        query: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Vec<serde_json::Value> {
        let headers: crate::imposter::Headers = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), vec![(*v).to_string()]))
            .collect();
        Imposter::generate_predicates_impl(
            &[generator],
            "POST",
            "/orders",
            &headers,
            body,
            query,
            TEST_PEER,
            &std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
        )
        .expect("predicate generation succeeds")
        .raw
    }

    #[test]
    fn jsonpath_captures_the_selected_value_and_scopes_the_predicate() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.id" } }),
            Some(r#"{"id": 42, "ts": "2026-10-08T10:00:00Z"}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({
                "deepEquals": { "body": "42" },
                "jsonpath": { "selector": "$.id" }
            })]
        );
    }

    #[test]
    fn jsonpath_with_several_matches_captures_an_array() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.items[*].sku" } }),
            Some(r#"{"items": [{"sku": "a"}, {"sku": "b"}]}"#),
            None,
            &[],
        );
        assert_eq!(predicates[0]["deepEquals"], json!({ "body": ["a", "b"] }));
    }

    #[test]
    fn jsonpath_selecting_nothing_captures_the_empty_string() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.missing" } }),
            Some(r#"{"id": 1}"#),
            None,
            &[],
        );
        assert_eq!(predicates[0]["deepEquals"], json!({ "body": "" }));
        assert_eq!(
            predicates[0]["jsonpath"],
            json!({ "selector": "$.missing" })
        );
    }

    #[test]
    fn xpath_with_ns_captures_the_selected_value() {
        let predicates = generate(
            json!({
                "matches": { "body": true },
                "xpath": { "selector": "//a:id", "ns": { "a": "urn:a" } },
                "caseSensitive": false
            }),
            Some(r#"<a:order xmlns:a="urn:a"><a:id>7</a:id><a:ts>now</a:ts></a:order>"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({
                "deepEquals": { "body": "7" },
                "xpath": { "selector": "//a:id", "ns": { "a": "urn:a" } },
                "caseSensitive": false
            })]
        );
    }

    /// Issue #1329: the selected value is recorded raw and `except` travels with the predicate,
    /// which strips both sides at match time.
    #[test]
    fn except_is_carried_beside_a_selected_scalar() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$.ref" }, "except": "-\\d+$" }),
            Some(r#"{"ref": "order-123"}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({
                "deepEquals": { "body": "order-123" },
                "jsonpath": { "selector": "$.ref" },
                "except": "-\\d+$"
            })]
        );
    }

    /// A selector that does not compile would make the recorded stub unloadable (#1220), so it is
    /// not carried: the body is captured whole, as before #1327.
    #[test]
    fn a_selector_that_does_not_compile_is_not_carried() {
        let predicates = generate(
            json!({ "matches": { "body": true }, "jsonpath": { "selector": "$[[[bad" } }),
            Some(r#"{"id": 1}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "body": r#"{"id": 1}"# } })]
        );
    }

    #[test]
    fn ignore_drops_query_keys_in_every_form() {
        for ignore in [json!("ts"), json!(["ts", "nonce"])] {
            let predicates = generate(
                json!({ "matches": { "query": true }, "ignore": { "query": ignore } }),
                None,
                Some("a=1&ts=99&nonce=x"),
                &[],
            );
            let expected = if ignore.is_string() {
                json!({ "query": { "a": "1", "nonce": "x" } })
            } else {
                json!({ "query": { "a": "1" } })
            };
            // An ignored key may vary, so the kept keys are matched with `equals` (issue #1329).
            assert_eq!(predicates[0]["equals"], expected, "ignore {ignore}");
            assert!(predicates[0].get("ignore").is_none());
        }
    }

    #[test]
    fn ignore_drops_a_header() {
        let predicates = generate(
            json!({
                "matches": { "headers": { "X-Tenant": true, "X-Request-Id": true } },
                "ignore": { "headers": "X-Request-Id" }
            }),
            None,
            None,
            &[("X-Tenant", "acme"), ("X-Request-Id", "r-1")],
        );
        assert_eq!(
            predicates[0]["equals"],
            json!({ "headers": { "X-Tenant": "acme" } })
        );
    }

    /// Mountebank recurses an object filter into object-valued fields; Rift's captured values are
    /// all strings, so an object-form filter removes nothing.
    #[test]
    fn ignore_object_form_recurses_only_into_objects() {
        let predicates = generate(
            json!({ "matches": { "query": true }, "ignore": { "query": { "a": "x" } } }),
            None,
            Some("a=1&b=2"),
            &[],
        );
        assert_eq!(
            predicates[0]["equals"],
            json!({ "query": { "a": "1", "b": "2" } })
        );
    }

    #[test]
    fn unread_generator_keys_are_listed() {
        assert_eq!(
            unread_generator_keys(
                &json!({ "matchs": { "path": true }, "matches": {}, "keyCaseSensitive": true })
            ),
            vec!["keyCaseSensitive", "matchs"]
        );
        assert!(
            unread_generator_keys(&json!({
                "matches": {}, "caseSensitive": true, "predicateOperator": "equals",
                "except": "x", "jsonpath": {}, "xpath": {}, "ignore": {}, "inject": "f"
            }))
            .is_empty()
        );
    }
}

/// Issue #1329: a generator records Mountebank's shape — one predicate per matched field, in a fixed
/// field order, with the generator's parameters copied onto each and the captured values raw.
#[cfg(test)]
mod predicate_generator_shape_tests {
    use super::*;
    use serde_json::json;

    fn generate(
        generator: serde_json::Value,
        body: Option<&str>,
        query: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Vec<serde_json::Value> {
        let headers: crate::imposter::Headers = headers
            .iter()
            .map(|(k, v)| ((*k).to_string(), vec![(*v).to_string()]))
            .collect();
        Imposter::generate_predicates_impl(
            &[generator],
            "POST",
            "/orders",
            &headers,
            body,
            query,
            TEST_PEER,
            &std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
        )
        .expect("predicate generation succeeds")
        .raw
    }

    /// What a replayed config's generators fail with: the config doors refuse these shapes, but a
    /// replay skips them, so the recorder must fail rather than record fewer predicates (#1413).
    fn generation_error(generators: serde_json::Value) -> String {
        let generators = generators.as_array().expect("a generator list").clone();
        match Imposter::generate_predicates_impl(
            &generators,
            "POST",
            "/orders",
            &crate::imposter::Headers::new(),
            None,
            None,
            TEST_PEER,
            &std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
        ) {
            Err(crate::scripting::PredicateGeneratorError::Invalid(message)) => message,
            other => panic!("expected an invalid-predicate failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_generator_fails_the_generation_instead_of_being_skipped() {
        let cases = [
            (json!(["x"]), "predicateGenerators[0] must be an object"),
            (
                json!([{"matches": {"path": true}}, {}]),
                "predicateGenerators[1] needs `matches` or `inject`",
            ),
            (
                json!([{"inject": 42}]),
                "predicateGenerators[0].inject must be a function string",
            ),
            (
                json!([{"matches": true}]),
                "predicateGenerators[0].matches must be an object of request fields",
            ),
            (
                json!([{"matches": {"path": true}, "caseSensitive": "yes"}]),
                "predicateGenerators[0].caseSensitive must be a boolean",
            ),
            (
                json!([{"matches": {"path": true}, "except": 5}]),
                "predicateGenerators[0].except must be a string",
            ),
            (
                json!([{"matches": {"pathh": true}}]),
                "predicateGenerators[0].matches field `pathh` is not a request field",
            ),
            (
                json!([{"matches": {"path": true}, "predicateOperator": 5}]),
                "predicateGenerators[0].predicateOperator `5` is not a predicate operator",
            ),
        ];
        for (generators, message) in cases {
            let error = generation_error(generators.clone());
            assert!(error.starts_with(message), "{generators}: {error}");
        }
    }

    /// Issue #1446: Mountebank records `requestFrom` and `ip` from the client socket. Rift records
    /// the strings its matcher compares (`handler.rs`: `client_addr` and `client_addr.ip()`).
    #[test]
    fn requestfrom_and_ip_are_recorded_from_the_client_address() {
        assert_eq!(
            generate(json!({"matches": {"ip": true}}), None, None, &[]),
            vec![json!({ "deepEquals": { "ip": "127.0.0.1" } })]
        );
        assert_eq!(
            generate(json!({"matches": {"requestFrom": true}}), None, None, &[]),
            vec![json!({ "deepEquals": { "requestFrom": "127.0.0.1:50000" } })]
        );
        assert_eq!(
            generate(
                json!({"matches": {"ip": true}, "predicateOperator": "exists"}),
                None,
                None,
                &[]
            ),
            vec![json!({ "exists": { "ip": true } })]
        );
    }

    const FORM: (&str, &str) = ("Content-Type", "application/x-www-form-urlencoded");

    #[test]
    fn a_form_is_recorded_whole_or_by_listed_keys() {
        assert_eq!(
            generate(
                json!({"matches": {"form": true}}),
                Some("a=1&b=2"),
                None,
                &[FORM]
            ),
            vec![json!({ "deepEquals": { "form": { "a": "1", "b": "2" } } })]
        );
        assert_eq!(
            generate(
                json!({"matches": {"form": {"a": true, "missing": true}}}),
                Some("a=1&b=2"),
                None,
                &[FORM]
            ),
            vec![json!({ "equals": { "form": { "a": "1" } } })]
        );
        assert_eq!(
            generate(
                json!({"matches": {"form": true}, "ignore": {"form": "b"}}),
                Some("a=1&b=2"),
                None,
                &[FORM]
            ),
            vec![json!({ "equals": { "form": { "a": "1" } } })]
        );
    }

    /// Mountebank has no `form` for a body that is not a non-empty urlencoded one, and records
    /// `deepEquals: {}`, which constrains nothing; Rift records no form predicate.
    #[test]
    fn a_request_without_a_form_records_no_form_predicate() {
        assert_eq!(
            generate(
                json!({"matches": {"form": true, "path": true}}),
                Some(r#"{"a":1}"#),
                None,
                &[("Content-Type", "application/json")]
            ),
            vec![json!({ "deepEquals": { "path": "/orders" } })]
        );
        assert_eq!(
            generate(
                json!({"matches": {"form": {"a": true}}}),
                Some(""),
                None,
                &[FORM]
            ),
            Vec::<serde_json::Value>::new(),
            "an empty urlencoded body has no form either"
        );
    }

    #[test]
    fn every_request_field_is_recorded_in_a_fixed_order() {
        let fields: Vec<String> = generate(
            json!({"matches": {
                "form": true, "ip": true, "requestFrom": true, "body": true,
                "headers": true, "query": true, "path": true, "method": true
            }}),
            Some("a=1"),
            Some("q=1"),
            &[FORM],
        )
        .iter()
        .map(|p| {
            p["deepEquals"]
                .as_object()
                .and_then(|o| o.keys().next().cloned())
                .expect("one field per predicate")
        })
        .collect();
        assert_eq!(
            fields,
            [
                "method",
                "path",
                "query",
                "headers",
                "body",
                "requestFrom",
                "ip",
                "form"
            ]
        );
    }

    /// `false` opts a field out, so naming an uncaptured field that way records the rest.
    #[test]
    fn an_uncaptured_field_opted_out_with_false_records_the_rest() {
        assert_eq!(
            generate(
                json!({"matches": {"path": true, "ip": false}}),
                None,
                None,
                &[]
            ),
            vec![json!({ "deepEquals": { "path": "/orders" } })]
        );
    }

    #[test]
    fn fields_are_emitted_in_a_fixed_order_with_parameters_on_each() {
        let predicates = generate(
            json!({
                "matches": {
                    "body": true,
                    "headers": { "X-Tenant": true },
                    "query": true,
                    "path": true,
                    "method": true
                },
                "caseSensitive": true,
                "except": "\\d+"
            }),
            Some("b1"),
            Some("a=1"),
            &[("x-tenant", "acme")],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "deepEquals": { "method": "POST" }, "caseSensitive": true, "except": "\\d+" }),
                json!({ "deepEquals": { "path": "/orders" }, "caseSensitive": true, "except": "\\d+" }),
                json!({ "deepEquals": { "query": { "a": "1" } }, "caseSensitive": true, "except": "\\d+" }),
                json!({ "equals": { "headers": { "X-Tenant": "acme" } }, "caseSensitive": true, "except": "\\d+" }),
                json!({ "deepEquals": { "body": "b1" }, "caseSensitive": true, "except": "\\d+" }),
            ]
        );
    }

    #[test]
    fn a_query_sub_key_matcher_records_equals_over_the_listed_keys_only() {
        let predicates = generate(
            json!({ "matches": { "query": { "a": true, "missing": true } } }),
            None,
            Some("a=1&b=2"),
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({ "equals": { "query": { "a": "1" } } })]
        );
    }

    // Mountebank's `buildEquals` records every listed key whatever its matcher value, so the
    // docs' `{"query": {"q": "mountebank"}}` records the request's `q` (issue #1580's mbTest docs
    // case); Rift used to record only the keys set to `true`, here `equals query {}`.
    #[test]
    fn a_listed_key_is_recorded_whatever_its_matcher_value() {
        let predicates = generate(
            json!({ "matches": { "query": { "q": "mountebank", "b": false } } }),
            None,
            Some("q=rift&b=2&c=3"),
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({ "equals": { "query": { "q": "rift", "b": "2" } } })]
        );
    }

    #[test]
    fn a_whole_field_matcher_records_an_absent_field_as_empty() {
        let predicates = generate(
            json!({ "matches": { "query": true, "body": true } }),
            None,
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "deepEquals": { "query": {} } }),
                json!({ "deepEquals": { "body": "" } }),
            ]
        );
    }

    #[test]
    fn whole_headers_are_recorded_with_deep_equals() {
        let predicates = generate(
            json!({ "matches": { "headers": true } }),
            None,
            None,
            &[("x-tenant", "acme")],
        );
        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "headers": { "x-tenant": "acme" } } })]
        );
    }

    #[test]
    fn exists_records_presence_per_field() {
        let predicates = generate(
            json!({
                "matches": { "path": true, "query": true, "headers": { "X-Tenant": true } },
                "predicateOperator": "exists"
            }),
            None,
            Some("a=1&b=2"),
            &[("x-tenant", "acme")],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "exists": { "path": true } }),
                json!({ "exists": { "query": { "a": true, "b": true } } }),
                json!({ "exists": { "headers": { "X-Tenant": true } } }),
            ]
        );
    }

    /// Mountebank stores the whole request under an explicit operator, once per matched field; Rift
    /// stores only that field's value (documented in docs/mountebank/proxy.md).
    #[test]
    fn an_explicit_operator_records_each_field_under_it() {
        let predicates = generate(
            json!({ "matches": { "path": true, "method": true }, "predicateOperator": "contains" }),
            None,
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "contains": { "method": "POST" } }),
                json!({ "contains": { "path": "/orders" } }),
            ]
        );
    }

    /// An explicit `equals` stays `equals`, on an object field too: the field's captured value
    /// under the operator, not the default `deepEquals`.
    #[test]
    fn an_explicit_equals_is_not_promoted_to_deep_equals() {
        let predicates = generate(
            json!({ "matches": { "query": true, "headers": { "X-Absent": true } },
                    "predicateOperator": "equals" }),
            None,
            Some("a=1"),
            &[],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "equals": { "query": { "a": "1" } } }),
                json!({ "equals": { "headers": {} } }),
            ]
        );
    }

    #[test]
    fn a_selector_scopes_only_the_body_predicate() {
        let predicates = generate(
            json!({ "matches": { "path": true, "body": true }, "jsonpath": { "selector": "$.id" } }),
            Some(r#"{"id": 42}"#),
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![
                json!({ "deepEquals": { "path": "/orders" } }),
                json!({ "deepEquals": { "body": "42" }, "jsonpath": { "selector": "$.id" } }),
            ]
        );
    }

    #[test]
    fn a_false_matcher_records_nothing() {
        let predicates = generate(
            json!({ "matches": { "path": false, "method": true } }),
            None,
            None,
            &[],
        );
        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "method": "POST" } })]
        );
    }

    /// proxyAlways merges a recording into the group whose predicate list equals its own; with one
    /// predicate per field that is a list comparison.
    #[test]
    fn proxy_always_groups_recordings_by_their_predicate_list() {
        let imposter = Imposter::new(
            serde_json::from_value(json!({
                "port": 0,
                "protocol": "http",
                "stubs": [{ "responses": [{ "proxy": { "to": "http://up", "mode": "proxyAlways" } }] }],
            }))
            .expect("valid imposter config"),
        )
        .expect("test imposter");
        let record = |path: &str, body: &str| {
            let headers: crate::imposter::Headers = crate::imposter::Headers::new();
            let predicates = Imposter::generate_predicates_impl(
                &[json!({ "matches": { "method": true, "path": true } })],
                "GET",
                path,
                &headers,
                None,
                None,
                TEST_PEER,
                &std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
            )
            .expect("generates")
            .raw;
            let stub: Stub = serde_json::from_value(json!({
                "predicates": predicates,
                "responses": [{ "is": { "statusCode": 200, "body": body } }],
            }))
            .expect("valid stub");
            imposter.insert_or_append_proxy_stub(stub, "http://up", "proxyAlways", None, None);
        };
        record("/a", "1");
        record("/b", "2");
        record("/a", "3");

        let stubs = imposter.get_stubs();
        let groups: Vec<(serde_json::Value, usize)> = stubs[1..]
            .iter()
            .map(|s| {
                (
                    serde_json::to_value(&s.predicates).expect("serializes"),
                    s.responses.len(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            // In recording order: a new proxyAlways recording goes at the end (issue #1389).
            vec![
                (
                    json!([{ "deepEquals": { "method": "GET" } }, { "deepEquals": { "path": "/a" } }]),
                    2
                ),
                (
                    json!([{ "deepEquals": { "method": "GET" } }, { "deepEquals": { "path": "/b" } }]),
                    1
                ),
            ]
        );
    }
}

/// Issue #1389: a recording is placed by the proxy stub that answered (its slot), as Mountebank
/// places it by `stubIndex()`; a `proxyAlways` one merges from after that stub, else goes last.
#[cfg(test)]
mod placement_tests {
    use super::*;
    use serde_json::{Value, json};

    fn imposter(stubs: Value) -> Imposter {
        Imposter::new(
            serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": stubs }))
                .expect("valid imposter config"),
        )
        .expect("test imposter")
    }

    fn proxy(path: &str, mode: &str) -> Value {
        json!({
            "predicates": [{ "equals": { "path": path } }],
            "responses": [{ "proxy": { "to": "http://up", "mode": mode } }],
        })
    }

    fn is(body: &str) -> Value {
        json!({ "responses": [{ "is": { "body": body } }] })
    }

    fn recording(path: &str, body: &str) -> Stub {
        serde_json::from_value(json!({
            "predicates": [{ "deepEquals": { "path": path } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }],
        }))
        .expect("valid stub")
    }

    fn slot_at(imposter: &Imposter, index: usize) -> u64 {
        imposter.snapshot().stubs()[index].slot
    }

    /// The first response body of each stub, or `proxy` for a proxy stub.
    fn layout(imposter: &Imposter) -> Vec<String> {
        imposter
            .get_stubs()
            .iter()
            .map(|s| {
                let v = serde_json::to_value(s).expect("serializes");
                let bodies: Vec<String> = v["responses"]
                    .as_array()
                    .expect("responses")
                    .iter()
                    .map(|r| match r.get("proxy") {
                        Some(_) => "proxy".to_string(),
                        None => r["is"]["body"].as_str().expect("body").to_string(),
                    })
                    .collect();
                bodies.join("+")
            })
            .collect()
    }

    #[test]
    fn a_proxy_once_recording_goes_in_front_of_the_proxy_that_answered() {
        let imp = imposter(json!([
            proxy("/b", "proxyOnce"),
            is("authored"),
            proxy("/a", "proxyOnce")
        ]));
        let answering = slot_at(&imp, 2);

        imp.insert_or_append_proxy_stub(
            recording("/a", "rec"),
            "http://up",
            "proxyOnce",
            Some(answering),
            None,
        );

        assert_eq!(layout(&imp), ["proxy", "authored", "rec", "proxy"]);
    }

    #[test]
    fn a_proxy_always_recording_is_appended_after_every_later_stub() {
        let imp = imposter(json!([proxy("/a", "proxyAlways"), is("authored")]));
        let answering = slot_at(&imp, 0);

        imp.insert_or_append_proxy_stub(
            recording("/a", "rec"),
            "http://up",
            "proxyAlways",
            Some(answering),
            None,
        );

        assert_eq!(layout(&imp), ["proxy", "authored", "rec"]);
    }

    // Mountebank searches for a merge target from the answering stub on, never before it.
    #[test]
    fn a_proxy_always_recording_merges_only_into_a_stub_after_the_proxy_that_answered() {
        let imp = imposter(json!([
            proxy("/b", "proxyAlways"),
            proxy("/a", "proxyAlways")
        ]));
        imp.insert_or_append_proxy_stub(
            recording("/a", "early"),
            "http://up",
            "proxyAlways",
            Some(slot_at(&imp, 0)),
            None,
        );
        assert_eq!(layout(&imp), ["proxy", "proxy", "early"]);

        // The second proxy answers: the recording after it merges.
        imp.insert_or_append_proxy_stub(
            recording("/a", "second"),
            "http://up",
            "proxyAlways",
            Some(slot_at(&imp, 1)),
            None,
        );
        assert_eq!(layout(&imp), ["proxy", "proxy", "early+second"]);
    }

    #[test]
    fn a_proxy_always_recording_does_not_merge_into_an_equal_stub_ahead_of_the_proxy() {
        let imp = imposter(json!([
            { "predicates": [{ "deepEquals": { "path": "/a" } }], "responses": [{ "is": { "body": "ahead" } }] },
            proxy("/a", "proxyAlways"),
        ]));
        let answering = slot_at(&imp, 1);

        imp.insert_or_append_proxy_stub(
            recording("/a", "rec"),
            "http://up",
            "proxyAlways",
            Some(answering),
            None,
        );

        assert_eq!(layout(&imp), ["ahead", "proxy", "rec"]);
    }

    // `addWaitBehavior` / `addDecorateBehavior` without generators record `predicates: []`;
    // Mountebank merges every such response into one stub instead of stacking catch-alls.
    #[test]
    fn proxy_always_recordings_with_no_predicates_merge_into_one_stub() {
        let imp = imposter(json!([proxy("/a", "proxyAlways")]));
        let answering = slot_at(&imp, 0);
        for body in ["1", "2", "3"] {
            let stub: Stub = serde_json::from_value(json!({
                "predicates": [],
                "responses": [{ "is": { "statusCode": 200, "body": body } }],
            }))
            .expect("valid stub");
            imp.insert_or_append_proxy_stub(
                stub,
                "http://up",
                "proxyAlways",
                Some(answering),
                None,
            );
        }

        assert_eq!(layout(&imp), ["proxy", "1+2+3"]);
    }

    // The answering stub was deleted while its request was in flight: fall back to the first
    // stub proxying to the same upstream.
    #[test]
    fn a_vanished_anchor_falls_back_to_the_first_proxy_to_the_upstream() {
        let imp = imposter(json!([
            is("authored"),
            proxy("/b", "proxyOnce"),
            proxy("/a", "proxyOnce")
        ]));
        let answering = slot_at(&imp, 2);
        imp.delete_stub(2).expect("delete");

        imp.insert_or_append_proxy_stub(
            recording("/a", "rec"),
            "http://up",
            "proxyOnce",
            Some(answering),
            None,
        );

        assert_eq!(layout(&imp), ["authored", "rec", "proxy"]);
    }

    // proxyAlways with a vanished anchor: the merge search runs after the first proxy to the
    // upstream, and with nothing equal there the recording goes last.
    #[test]
    fn a_vanished_proxy_always_anchor_merges_after_the_first_proxy_to_the_upstream() {
        let imp = imposter(json!([
            proxy("/b", "proxyAlways"),
            proxy("/a", "proxyAlways")
        ]));
        let answering = slot_at(&imp, 1);
        imp.insert_or_append_proxy_stub(
            recording("/a", "first"),
            "http://up",
            "proxyAlways",
            Some(answering),
            None,
        );
        imp.delete_stub(1).expect("delete");

        imp.insert_or_append_proxy_stub(
            recording("/a", "second"),
            "http://up",
            "proxyAlways",
            Some(answering),
            None,
        );
        imp.insert_or_append_proxy_stub(
            recording("/c", "third"),
            "http://up",
            "proxyAlways",
            Some(answering),
            None,
        );

        assert_eq!(layout(&imp), ["proxy", "first+second", "third"]);
    }

    // No anchor at all (no stub answered) and no proxy stub: the recording goes last.
    #[test]
    fn with_no_anchor_and_no_proxy_stub_a_recording_goes_last() {
        let imp = imposter(json!([is("authored")]));

        imp.insert_or_append_proxy_stub(
            recording("/a", "rec"),
            "http://up",
            "proxyOnce",
            None,
            None,
        );

        assert_eq!(layout(&imp), ["authored", "rec"]);
    }
}

/// Issue #1386: a recorded stub carries the `proxyOnce` claim it was recorded under, and the claim
/// goes when the stub does — or when the stub's content does.
#[cfg(test)]
mod claim_tests {
    use super::*;
    use serde_json::json;

    fn imposter() -> Imposter {
        Imposter::new(
            serde_json::from_value(json!({
                "port": 0,
                "protocol": "http",
                "stubs": [{ "responses": [{ "proxy": { "to": "http://up", "mode": "proxyOnce" } }] }],
            }))
            .expect("valid imposter config"),
        )
        .expect("test imposter")
    }

    fn stub(body: &str) -> Stub {
        serde_json::from_value(json!({
            "predicates": [{ "deepEquals": { "path": "/a" } }],
            "responses": [{ "is": { "statusCode": 200, "body": body } }],
        }))
        .expect("valid stub")
    }

    /// Records `sig` in the store and inserts its stub in front of the proxy, as the engine does.
    fn record(imp: &Imposter) -> RequestSignature {
        let sig = RequestSignature::new("POST", "/a", None, &[]);
        let port = imp.journal_port();
        let token = match imp
            .proxy_store
            .try_claim_as(port, &sig, ProxyMode::ProxyOnce)
            .expect("claim")
        {
            ClaimOutcome::Claimed(token) => token,
            other => panic!("expected a claim, got {other:?}"),
        };
        imp.proxy_store
            .record(
                port,
                sig.clone(),
                token,
                crate::recording::RecordedResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"up".to_vec(),
                    latency_ms: None,
                    timestamp_secs: 0,
                },
            )
            .expect("record");
        imp.insert_or_append_proxy_stub(
            stub("up"),
            "http://up",
            "proxyOnce",
            None,
            Some(sig.clone()),
        );
        sig
    }

    fn recorded(imp: &Imposter, sig: &RequestSignature) -> bool {
        imp.proxy_store
            .try_claim_as(imp.journal_port(), sig, ProxyMode::ProxyOnce)
            .expect("claim")
            == ClaimOutcome::AlreadyRecorded
    }

    #[test]
    fn replacing_a_recording_with_new_content_forgets_its_claim() {
        let imp = imposter();
        let sig = record(&imp);

        imp.replace_stub(0, stub("edited")).expect("replace");

        assert!(!recorded(&imp, &sig));
    }

    #[test]
    fn replacing_a_recording_with_identical_content_keeps_its_claim() {
        let imp = imposter();
        let sig = record(&imp);

        imp.replace_stub(0, stub("up")).expect("replace");

        assert!(recorded(&imp, &sig));
    }

    #[test]
    fn moving_a_recording_keeps_its_claim() {
        let imp = imposter();
        let sig = record(&imp);

        imp.move_stub(0, 1).expect("move");

        assert!(recorded(&imp, &sig));
    }

    #[test]
    fn deleting_a_recording_forgets_its_claim() {
        let imp = imposter();
        let sig = record(&imp);

        imp.delete_stub(0).expect("delete");

        assert!(!recorded(&imp, &sig));
    }

    // A stub edit that leaves the recording alone keeps its claim.
    #[test]
    fn deleting_another_stub_keeps_the_claim() {
        let imp = imposter();
        let sig = record(&imp);

        imp.delete_stub(1).expect("delete the proxy stub");

        assert!(recorded(&imp, &sig));
    }
}

/// The create-time dry run of `predicateGenerators` injects (issue #1580).
#[cfg(all(test, feature = "javascript"))]
mod dry_run_tests {
    use super::*;
    use serde_json::json;

    fn stubs_with(mode: &str, generators: serde_json::Value) -> Vec<Stub> {
        vec![
            serde_json::from_value(json!({
                "responses": [{ "proxy": {
                    "to": "http://up", "mode": mode, "predicateGenerators": generators,
                } }],
            }))
            .expect("valid stub"),
        ]
    }

    fn inject(script: &str) -> Vec<Stub> {
        stubs_with("proxyOnce", json!([{ "inject": script }]))
    }

    async fn dry_run(stubs: &[Stub], timeout: Duration) -> Result<(), GeneratorDryRun> {
        let files = Arc::new(crate::imposter::ScriptFileAccess::network(None));
        dry_run_predicate_generators(stubs, timeout, files).await
    }

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn a_throwing_inject_is_refused_with_its_source_and_message() {
        let script = "function (config) { throw new Error('boom'); }";
        let refusal = dry_run(&inject(script), TIMEOUT).await.unwrap_err();
        let GeneratorDryRun::Refused { source, data } = refusal else {
            panic!("expected a refusal, got {refusal:?}");
        };
        assert_eq!(source, format!("({script})(config);"));
        // The thrown error's message alone, as Mountebank reports it.
        assert_eq!(data.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn an_inject_returning_undefined_is_refused() {
        let refusal = dry_run(&inject("function (config) {}"), TIMEOUT).await;
        assert!(
            matches!(refusal, Err(GeneratorDryRun::Refused { .. })),
            "{refusal:?}"
        );
    }

    #[tokio::test]
    async fn an_inject_returning_an_empty_array_is_admitted() {
        let admitted = dry_run(&inject("function (config) { return []; }"), TIMEOUT).await;
        assert_eq!(admitted, Ok(()));
    }

    #[tokio::test]
    async fn an_inject_guarded_on_is_dry_run_is_admitted() {
        let script = "function (config) { if (config.request.isDryRun) { return []; } \
                      return [{ equals: JSON.parse(config.request.body) }]; }";
        assert_eq!(dry_run(&inject(script), TIMEOUT).await, Ok(()));
        // Unguarded, the same parse throws on the synthetic empty body, as in Mountebank.
        let unguarded =
            "function (config) { return [{ equals: JSON.parse(config.request.body) }]; }";
        assert!(dry_run(&inject(unguarded), TIMEOUT).await.is_err());
    }

    #[tokio::test]
    async fn the_inject_sees_mountebanks_synthetic_request() {
        let script = "function (config) { var r = config.request; \
            if (r.method !== 'GET' || r.path !== '/' || r.body !== '' || r.requestFrom !== '' \
                || 'ip' in r || Object.keys(r.query).length || Object.keys(r.headers).length \
                || Object.keys(r.form).length || r.isDryRun !== true) { throw new Error(JSON.stringify(r)); } \
            return []; }";
        assert_eq!(dry_run(&inject(script), TIMEOUT).await, Ok(()));
    }

    #[tokio::test]
    async fn a_matches_only_list_never_reaches_the_script_pool() {
        // A zero deadline would refuse any run that went through the pool.
        let stubs = stubs_with("proxyOnce", json!([{ "matches": { "path": true } }]));
        assert_eq!(dry_run(&stubs, Duration::ZERO).await, Ok(()));
    }

    #[tokio::test]
    async fn a_generated_predicate_the_engine_would_refuse_is_not_a_create_time_refusal() {
        let script = "function (config) { return [{ notAnOperator: {} }]; }";
        assert_eq!(dry_run(&inject(script), TIMEOUT).await, Ok(()));
    }

    #[tokio::test]
    async fn an_inject_past_the_script_timeout_is_refused() {
        let script = "function (config) { var end = Date.now() + 2000; while (Date.now() < end) {} return []; }";
        let refusal = dry_run(&inject(script), Duration::from_millis(50)).await;
        let Err(GeneratorDryRun::Refused { data, .. }) = refusal else {
            panic!("expected a refusal, got {refusal:?}");
        };
        assert_eq!(data.as_deref(), Some("timed out after 50ms"));
    }

    #[tokio::test]
    async fn every_proxy_mode_is_checked_proxy_transparent_included() {
        let throwing = json!([{ "inject": "function () { throw new Error('boom'); }" }]);
        for mode in ["proxyOnce", "proxyAlways", "proxyTransparent"] {
            let refusal = dry_run(&stubs_with(mode, throwing.clone()), TIMEOUT).await;
            assert!(refusal.is_err(), "{mode}: {refusal:?}");
        }
    }

    #[tokio::test]
    async fn response_and_predicate_injects_are_not_dry_run() {
        let stubs: Vec<Stub> = vec![
            serde_json::from_value(json!({
                "predicates": [{ "inject": "function () { throw new Error('p'); }" }],
                "responses": [{ "inject": "function () { throw new Error('r'); }" }],
            }))
            .expect("valid stub"),
        ];
        assert_eq!(dry_run(&stubs, Duration::ZERO).await, Ok(()));
    }
}
