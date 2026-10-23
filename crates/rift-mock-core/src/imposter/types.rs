//! Type definitions for Mountebank-compatible imposter management.
//!
//! This module contains all the structs, enums, and type aliases used by the imposter system.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Issue #936: the Mountebank wire-shape tolerance helpers moved to `rift-types::wire` so the
// intercept rule schema can share them — the imposter path and the intercept path parse the
// same JSON and must agree about what it means. Re-exported here so every
// `#[serde(with = "multi_value_headers")]` attribute path in this file resolves unchanged.
pub(crate) use rift_types::wire::{multi_value_headers, single_value_headers};

/// A header object, in the order it was written or received (issue #1606). Mountebank keeps that
/// order — a request's `headers` in the order the client sent them, a response's in the order the
/// stub (or a decorate/inject) wrote them — and a script can `Object.keys` it, so a hash map's
/// arbitrary, per-map order is visible. One entry per case-folded name (see
/// [`multi_value_headers`]); a repeated name holds every value at its first position. The default
/// (SipHash) hasher: journals and stub configs are long-lived and keyed by input, which the HashDoS
/// policy in `util::fastmap` reserves SipHash for.
pub type Headers = indexmap::IndexMap<String, Vec<String>>;

/// A header object with one value per name (`proxy.injectHeaders`, `_rift.fault.error.headers`,
/// the debug endpoint), in the order it was written (issue #1606).
pub type SingleHeaders = indexmap::IndexMap<String, String>;

// ============================================================================
// Recorded Request Types
// ============================================================================

/// Serde for a recorded query (issue #1553): a key sent once is a string and a repeated key an
/// array, as Mountebank's `querystring.parse` shows them. Unlike [`multi_value_headers`], keys are
/// case-sensitive — `?Q=1&q=2` is two parameters — so nothing is merged. Decoding accepts either
/// shape for any key, so journals and exports written before the array form still read.
pub(crate) mod multi_value_query {
    use indexmap::IndexMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    // The default (SipHash) hasher: the journal is long-lived and keyed by request input, which the
    // HashDoS policy in `util::fastmap` reserves SipHash for.
    pub type Query = IndexMap<String, Vec<String>>;

    #[derive(Serialize)]
    #[serde(untagged)]
    enum Shown<'a> {
        One(&'a str),
        Many(&'a [String]),
    }

    // Generic over the hasher: `form` (issue #1602) prints through here from a `MultiValueMap`.
    pub fn serialize<S: Serializer, H>(
        query: &IndexMap<String, Vec<String>, H>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Request order, array-index keys first, as a JS object lists them (issue #1596).
        serializer.collect_map(crate::util::js_key_order(query).into_iter().filter_map(
            |(key, values)| {
                let shown = match values.as_slice() {
                    [] => return None,
                    [one] => Shown::One(one),
                    many => Shown::Many(many),
                };
                Some((key, shown))
            },
        ))
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Query, D::Error> {
        let raw = IndexMap::<String, OneOrMany>::deserialize(deserializer)?;
        Ok(raw
            .into_iter()
            .map(|(key, value)| {
                let values = match value {
                    OneOrMany::One(one) => vec![one],
                    OneOrMany::Many(many) => many,
                };
                (key, values)
            })
            .filter(|(_, values)| !values.is_empty())
            .collect())
    }
}

/// A recorded request's headers in request order, array-index names first, as a JS object lists
/// them (issues #1596, #1606); a single value is a string and several an array.
fn serialize_recorded_headers<H, S>(headers: &H, serializer: S) -> Result<S::Ok, S::Error>
where
    H: std::ops::Deref<Target = Headers>,
    S: serde::Serializer,
{
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Shown<'a> {
        One(&'a str),
        Many(&'a [String]),
    }
    serializer.collect_map(
        crate::util::js_key_order(&**headers)
            .into_iter()
            .filter_map(|(name, values)| {
                let shown = match values.as_slice() {
                    [] => return None, // a name with no values was never a header line; omit it
                    [one] => Shown::One(one),
                    many => Shown::Many(many),
                };
                Some((name, shown))
            }),
    )
}

fn boxed_single_value_headers<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Box<SingleHeaders>, D::Error> {
    single_value_headers::deserialize(d).map(Box::new)
}

/// Recorded request for imposter
///
/// `Serialize` is hand-written (issue #1602): Mountebank prints `ip` on every recorded request and
/// `form` on a urlencoded one, and both are functions of fields held here, so they are derived
/// when the entry is written out rather than stored (no journal byte cost, no stored-format
/// change). `Deserialize` ignores an incoming `ip`/`form` for the same reason.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedRequest {
    pub request_from: String,
    pub method: String,
    pub path: String,
    /// Every value of each query key, in request order (issue #1553). On the wire a key sent once
    /// is a string and a repeated key an array, as Mountebank prints `requests[].query`; a journal
    /// written when Rift comma-joined a repeated key still decodes, its value as one string.
    #[serde(with = "multi_value_query")]
    pub query: multi_value_query::Query,
    /// Every header in the order the client sent it (issue #1606), as Mountebank's
    /// `headersMap.ofRaw` records `requests[].headers`.
    /// Printed by [`RecordedRequestWire`], in [`js_key_order`](crate::util::js_key_order).
    #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
    pub headers: Headers,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The body was larger than the journal's whole byte budget, so the request was recorded
    /// without it rather than dropped (issue #1441). A Rift extension, like `_mode`: absent (and
    /// `false`) for every request recorded with its body, so existing output is unchanged.
    #[serde(
        rename = "_bodyOmitted",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub body_omitted: bool,
    /// Whether `body` is UTF-8 text or a base64-encoded binary body (issue #636), mirroring
    /// `IsResponse::mode` (issue #117). `#[serde(default)]` so recordings from before this field
    /// existed deserialize as `Text`; omitted on serialize for a text body so recordings and
    /// wire traffic that are all text stay byte-identical to pre-#636 output.
    #[serde(rename = "_mode", default, skip_serializing_if = "is_text_mode")]
    pub mode: ResponseMode,
    pub timestamp: String,
    /// Why this request matched — or did not. Captured at request time and attached to this
    /// entry once matching has finished, so an operator reading the journal can answer "why did
    /// this not match?" without re-deriving the scan by hand.
    ///
    /// `None` (and therefore absent on the wire, like `_mode` above) whenever no outcome was
    /// recorded: recording was off, the backend has no stable indices to attach to, the request
    /// took the `X-Rift-Debug` path, or matching itself errored. Absence means "not recorded",
    /// never "did not match".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_outcome: Option<MatchOutcome>,
    /// The status that went back, and how long the imposter took to produce it (issue #364).
    ///
    /// Attached after the response exists, like [`Self::match_outcome`] and for the same reason:
    /// the entry is journalled *before* matching, so neither is knowable at record time. They are
    /// two fields rather than one because the console renders them as two columns, and one being
    /// present without the other is not a state that can occur — both are attached together or
    /// neither is.
    ///
    /// Absent means **not recorded**, never zero. A `0` here would read as "instant" and a
    /// missing status as "no response"; both are claims this engine cannot make about a request
    /// whose answer it did not send (#1227): an `X-Rift-Debug` report describes the request rather
    /// than answering it, a TCP fault aborts the connection instead of responding, and a request
    /// abandoned before a response existed never reaches the attach. An error response the client
    /// does receive is an answer like any other and carries its status.
    ///
    /// A present `latency_ms` of `0` is an ordinary reading, not a missing one: a stub answered
    /// from memory usually takes well under a millisecond. The resolution is chosen for the
    /// question the column exists to answer — *is this mock the slow thing?* — where the
    /// interesting values are `behaviors.wait` delays and proxied upstreams, both far above 1 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Which node served this request, when something clustered is embedding this engine.
    ///
    /// **Never set by this crate.** Single-node Rift has exactly one node and no name for it, so
    /// the field exists here only because the journal entry is this crate's type: a
    /// [`RequestJournal`](crate::imposter::RequestJournal) implementation that spans nodes stamps
    /// it in its own `record`, where the node's identity is actually known.
    ///
    /// A `String`, not a numeric id: node identity is an identifier and never a magnitude, and an
    /// embedder whose ids exceed 2^53 would otherwise have them silently rounded by any JavaScript
    /// reading the journal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// The `timestamp` a recorded request is stamped with: `Date.prototype.toJSON()`'s shape, which
/// is what Mountebank records (issue #1610). Always millisecond precision and a literal `Z`, so
/// the width is fixed (`to_rfc3339` prints `+00:00` and a fraction of varying length). The field
/// stays an opaque `String` so journals holding the older microsecond form still decode.
#[must_use]
pub(crate) fn journal_timestamp(now: chrono::DateTime<chrono::Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl RecordedRequest {
    /// The client IP an `ip` predicate matches against, recovered from `request_from`
    /// (`ip:port`). Falls back to a bare IP, which tests and embedders record. `None` when
    /// `request_from` is neither: "no client info", so `ip` is omitted rather than invented.
    #[must_use]
    pub fn client_ip(&self) -> Option<String> {
        self.request_from
            .parse::<std::net::SocketAddr>()
            .map(|addr| addr.ip())
            .or_else(|_| self.request_from.parse::<std::net::IpAddr>())
            .ok()
            .map(|ip| ip.to_string())
    }

    /// `request.form` as Mountebank prints it; never for a binary or body-less entry.
    fn form(&self) -> Option<crate::util::MultiValueMap> {
        if self.body_omitted || !is_text_mode(&self.mode) {
            return None;
        }
        crate::imposter::Imposter::request_form(&self.headers, self.body.as_deref())
    }
}

/// Borrowed wire shape of [`RecordedRequest`]: every stored field plus the derived `ip`/`form`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordedRequestWire<'a> {
    request_from: &'a str,
    method: &'a str,
    path: &'a str,
    #[serde(with = "multi_value_query")]
    query: &'a multi_value_query::Query,
    #[serde(serialize_with = "serialize_recorded_headers")]
    headers: &'a Headers,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", with = "form_wire")]
    form: Option<crate::util::MultiValueMap>,
    #[serde(rename = "_bodyOmitted", skip_serializing_if = "std::ops::Not::not")]
    body_omitted: bool,
    #[serde(rename = "_mode", skip_serializing_if = "is_text_mode")]
    mode: &'a ResponseMode,
    timestamp: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_outcome: Option<&'a MatchOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<&'a str>,
}

/// `form` prints like `query`: a key sent once is a string, a repeated key an array.
mod form_wire {
    use serde::Serializer;

    #[allow(clippy::ref_option)] // serde's `with` passes `&Option<T>`
    pub fn serialize<S: Serializer>(
        form: &Option<crate::util::MultiValueMap>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match form {
            Some(map) => super::multi_value_query::serialize(map, serializer),
            None => serializer.serialize_none(),
        }
    }
}

impl Serialize for RecordedRequest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RecordedRequestWire {
            request_from: &self.request_from,
            method: &self.method,
            path: &self.path,
            query: &self.query,
            headers: &self.headers,
            body: self.body.as_deref(),
            ip: self.client_ip(),
            form: self.form(),
            body_omitted: self.body_omitted,
            mode: &self.mode,
            timestamp: &self.timestamp,
            match_outcome: self.match_outcome.as_ref(),
            status: self.status,
            latency_ms: self.latency_ms,
            node: self.node.as_deref(),
        }
        .serialize(serializer)
    }
}

/// Cap on the candidates named in [`MatchOutcome::tried`]; the rest are counted in
/// [`MatchOutcome::tried_omitted`].
///
/// An outcome rides on every journal entry and the journal ring holds
/// up to [`DEFAULT_JOURNAL_MAX_BYTES`](crate::imposter::DEFAULT_JOURNAL_MAX_BYTES) of entries per
/// port, so an uncapped list would let one imposter with a few hundred stubs multiply the journal's
/// footprint by that stub count. Twenty-five is well past the point where reading the list by hand stops
/// being how you diagnose a miss.
pub const MAX_TRIED_STUBS: usize = 25;

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if contract
fn is_zero(count: &u32) -> bool {
    *count == 0
}

/// The compact record of one request's match, attached to its journal entry.
///
/// `tried` lists only the candidates the matcher actually VISITED, in visit order. The Stage-1
/// index (`core::stub_index`) prunes stubs it can prove cannot match before the scan begins, and
/// those are never evaluated — listing them would claim a verdict the matcher never reached. A
/// stub's absence from `tried` therefore means "ruled out by the index, or never reached because
/// an earlier stub already won", not "passed".
///
/// A miss:
///
/// ```json
/// { "matched": false,
///   "tried": [{ "stubIndex": 0, "stubId": "users",
///               "why": { "reason": "failedPredicate", "predicateIndex": 1 } }] }
/// ```
///
/// A hit, where `tried` holds only the candidates visited BEFORE the winner:
///
/// ```json
/// { "matched": true, "stubIndex": 2, "stubId": "fallback", "tried": [] }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchOutcome {
    pub matched: bool,
    /// Position of the winning stub in the imposter's stub list; absent when nothing matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_index: Option<usize>,
    /// The winning stub's `id`, when it declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tried: Vec<TriedStub>,
    /// Candidates visited past [`MAX_TRIED_STUBS`] and therefore not named above. Counted rather
    /// than dropped: a silently truncated `tried` would make "these are the stubs that were
    /// tried" false with nothing on the wire to say so.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tried_omitted: u32,
}

/// One candidate the matcher visited and did not serve, with the reason it fell out.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriedStub {
    pub stub_index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<String>,
    pub why: TriedWhy,
}

/// Why a visited candidate did not win.
///
/// Adjacently tagged, so every variant is one flat object with the same `reason` key and consumers
/// can switch on it without type-testing the value:
///
/// ```json
/// { "reason": "skippedSpace" }
/// { "reason": "skippedScenarioState" }
/// { "reason": "failedPredicate", "predicateIndex": 1 }
/// ```
///
/// (An externally tagged enum would render the first two as the bare string `"skippedSpace"` and
/// the third as `{ "failedPredicate": 1 }` — the same field alternating between a string and an
/// object, which is exactly the shape that makes a consumer branch on JSON types.)
///
/// The failure datum is the predicate's INDEX in the stub's `predicates`, not a field path: a
/// [`Predicate`]'s field is a key inside its operation's map (and `and`/`or`/`not` nest further),
/// so there is no single path to name. A consumer renders the offending predicate by indexing the
/// stub config it already has.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reason", content = "predicateIndex", rename_all = "camelCase")]
pub enum TriedWhy {
    /// Correlated-isolation gate (issue #223): the stub is scoped to a space this request's
    /// resolved `flow_id` is not. Never reached predicate evaluation.
    SkippedSpace,
    /// Scenario FSM gate: the stub's `requiredScenarioState` is not the current state. Never
    /// reached predicate evaluation either.
    SkippedScenarioState,
    /// The first predicate (by position in the stub's `predicates`) that the request failed.
    /// Predicates are implicitly AND-ed and the scan short-circuits, so later ones were never
    /// evaluated.
    FailedPredicate(usize),
}

// ============================================================================
// Debug Mode Structures (Rift Extension)
// ============================================================================

/// Debug response for X-Rift-Debug header (Rift extension)
/// Returns match information instead of executing the response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugResponse {
    pub debug: bool,
    pub request: DebugRequest,
    pub imposter: DebugImposter,
    pub match_result: DebugMatchResult,
}

/// Debug request information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugRequest {
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub headers: SingleHeaders,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

/// Debug imposter information
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugImposter {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub protocol: String,
    pub stub_count: usize,
}

/// Debug match result
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugMatchResult {
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicates: Option<Vec<Predicate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_preview: Option<DebugResponsePreview>,
    /// All stubs for inspection when no match found
    #[serde(skip_serializing_if = "Option::is_none")]
    pub all_stubs: Option<Vec<DebugStubInfo>>,
    /// Reason for no match
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Debug response preview (subset of actual response)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugResponsePreview {
    pub response_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<SingleHeaders>,
    /// Truncated body preview (first 500 chars)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_preview: Option<String>,
}

/// Debug stub info for listing all stubs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugStubInfo {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub predicates: Vec<Predicate>,
    pub response_count: usize,
}

// ============================================================================
// Stub Types
// ============================================================================

/// Stub definition (Mountebank-compatible with Rift extensions)
/// Field ordering matches Mountebank output: name, scenarioName, predicates, responses, _links
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", try_from = "StubRaw")]
pub struct Stub {
    /// A label for the reader (Mountebank compatible, issue #1362), kept and printed back. Never
    /// matched on: the matcher, the scenario gate and stub analysis do not read it. Printed first,
    /// where a label reads. Hashed into `stub_key` like any other field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional scenario name for documentation/organization (Mountebank compatible)
    /// Placed after `name` to match Mountebank output ordering
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario_name: Option<String>,
    /// Scenario FSM gate (WireMock `whenScenarioStateIs`): the stub is eligible only when the
    /// `(flow_id, scenario_name)` state equals this. Absent ⇒ always eligible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_scenario_state: Option<String>,
    /// Scenario FSM transition (WireMock `willSetStateTo`): after this stub responds, set the
    /// `(flow_id, scenario_name)` state to this. Absent ⇒ no transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_scenario_state: Option<String>,
    /// Correlated-isolation scope (issue #223): when set, the stub is eligible only for requests
    /// whose resolved `flow_id` (see `flowIdSource`) equals this. Absent ⇒ global (matches any
    /// space), preserving PerInstance behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Optional unique identifier for the stub (Rift extension)
    /// Useful for targeting specific stubs for updates/deletion without relying on index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Route pattern for path-parameter extraction (issue #433, Rift extension), e.g.
    /// `/users/:id`. When set and the request path matches its shape, each `:name` segment
    /// populates `request.pathParams.<name>` for response templates and scripts. Absent ⇒ no path
    /// params (unchanged default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_pattern: Option<String>,
    /// Not printed when empty (issue #1361): an absent `predicates` decodes to the same `[]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub predicates: Vec<Predicate>,
    #[serde(default)]
    pub responses: Vec<StubResponse>,
    /// Upstream URL recorded from during proxy recording (Mountebank compatible)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_from: Option<String>,
    /// Engine-ignored verification annotation (issue #251). Preserved verbatim across the load
    /// round-trip so `rift-verify --verify-dynamic` can read the declared expectation sequence
    /// back from `GET /imposters`; the engine never acts on it.
    #[serde(rename = "_verify", skip_serializing_if = "Option::is_none")]
    pub verify: Option<serde_json::Value>,
}

/// What decides a stub's eligibility before its predicates run (issue #1308): the correlation
/// space and the scenario gate. A stub with no `requiredScenarioState` has no scenario gate,
/// whatever its `scenarioName` says (that name alone only sets state).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Gate<'a> {
    space: Option<&'a str>,
    /// `(scenario_name, required_scenario_state)`
    scenario: Option<(&'a str, &'a str)>,
}

impl Stub {
    /// The gates the matcher evaluates before this stub's predicates.
    pub(crate) fn gate(&self) -> Gate<'_> {
        Gate {
            space: self.space.as_deref(),
            scenario: self
                .required_scenario_state
                .as_deref()
                .map(|state| (self.scenario_name.as_deref().unwrap_or(""), state)),
        }
    }

    /// Drop every response's `_proxyResponseTime`, as Mountebank's replayable form does (issue
    /// #1519): what is replayed is no longer a saved proxy response.
    pub fn forget_proxy_response_times(&mut self) {
        for response in &mut self.responses {
            if let StubResponse::Is { is, .. } = response {
                is.proxy_response_time = None;
            }
        }
    }
}

impl Gate<'_> {
    /// No space and no scenario gate: eligible for every request, so it covers every gate.
    pub(crate) fn is_open(&self) -> bool {
        self.space.is_none() && self.scenario.is_none()
    }

    /// `self`'s stub is eligible whenever `other`'s is: no space or the same space, and no
    /// scenario gate or the same one. The relation an earlier stub needs to shadow a later one.
    pub(crate) fn covers(&self, other: &Gate<'_>) -> bool {
        (self.space.is_none() || self.space == other.space)
            && (self.scenario.is_none() || self.scenario == other.scenario)
    }
}

/// Raw deserialization type for Stub — handles alternative field names and format conversions:
/// - `rules` as an alias for `predicates`
/// - `delayRange` array (stub-level latency) converted to per-response `wait` behavior
/// - `recordedFrom` URL from Mountebank proxy recording
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        description = "A stub: the predicates a request must match and the responses it then cycles through.",
        transform = crate::schema::stub_requires_responses
    )
)]
#[serde(rename_all = "camelCase")]
struct StubRaw {
    /// A label for the reader, printed back and never matched on.
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scenario_name: Option<String>,
    #[serde(default)]
    required_scenario_state: Option<String>,
    #[serde(default)]
    new_scenario_state: Option<String>,
    #[serde(default)]
    space: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    route_pattern: Option<String>,
    #[serde(default)]
    predicates: Vec<Predicate>,
    /// Alternative field name "rules" used instead of "predicates" in some recorded formats
    #[serde(default)]
    rules: Vec<Predicate>,
    #[serde(default)]
    responses: Vec<StubResponse>,
    #[serde(default)]
    recorded_from: Option<String>,
    /// Stub-level latency range: `[{ "min": "50", "max": "100" }]`
    #[serde(default)]
    delay_range: Vec<DelayRange>,
    /// Engine-ignored verification annotation, preserved verbatim (issue #251).
    #[serde(default, rename = "_verify")]
    verify: Option<serde_json::Value>,
}

/// A `delayRange` entry for stub-level latency configuration.
/// Both `min` and `max` may be numbers or numeric strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "DelayRangeRaw")]
struct DelayRange {
    min: u64,
    max: u64,
}

/// The wire shape. `DelayRange` is rewritten into a `wait` by the `TryFrom<StubRaw>` conversion, so an
/// inverted range has to be refused here, at parse, or it reaches the draw and panics (issue #1148).
#[derive(Debug, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        description = "A stub-level latency range in milliseconds, rewritten into a `wait` on every response."
    )
)]
struct DelayRangeRaw {
    #[serde(deserialize_with = "de_u64_or_string")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::U64OrNumericString")
    )]
    min: u64,
    #[serde(deserialize_with = "de_u64_or_string")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::U64OrNumericString")
    )]
    max: u64,
}

impl TryFrom<DelayRangeRaw> for DelayRange {
    type Error = String;

    fn try_from(raw: DelayRangeRaw) -> Result<Self, Self::Error> {
        if raw.min > raw.max {
            return Err(format!(
                "`delayRange` has min {} greater than max {}",
                raw.min, raw.max
            ));
        }
        Ok(Self {
            min: raw.min,
            max: raw.max,
        })
    }
}

fn de_u64_or_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    use serde::de::Error;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| D::Error::custom("expected non-negative integer")),
        serde_json::Value::String(s) => s
            .parse::<u64>()
            .map_err(|_| D::Error::custom(format!("cannot parse '{s}' as integer"))),
        _ => Err(D::Error::custom("expected number or numeric string")),
    }
}

impl TryFrom<StubRaw> for Stub {
    type Error = String;

    fn try_from(raw: StubRaw) -> Result<Self, Self::Error> {
        // "rules" is an alias for "predicates"; prefer "predicates" when both present
        let predicates = if !raw.predicates.is_empty() {
            raw.predicates
        } else {
            raw.rules
        };
        if admission() == Admission::Checked {
            refuse_empty_responses(&raw.responses)?;
            validate_predicates(&predicates)?;
        }

        // Convert stub-level delayRange to a wait behavior injected into each response
        let responses = if raw.delay_range.is_empty() {
            raw.responses
        } else {
            let wait_val = build_wait_from_delay_range(&raw.delay_range);
            raw.responses
                .into_iter()
                .map(|r| inject_wait_behavior(r, wait_val.clone()))
                .collect()
        };

        Ok(Stub {
            name: raw.name,
            scenario_name: raw.scenario_name,
            required_scenario_state: raw.required_scenario_state,
            new_scenario_state: raw.new_scenario_state,
            space: raw.space,
            id: raw.id,
            route_pattern: raw.route_pattern,
            predicates,
            responses,
            recorded_from: raw.recorded_from,
            verify: raw.verify,
        })
    }
}

/// Whether the decode running on this thread applies the engine's admission checks (issue #1267).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// A config door: refuse what the engine would not admit today.
    Checked,
    /// [`deserialize_replayed`]: structure and normalisation only.
    Skipped,
}

thread_local! {
    static ADMISSION: std::cell::Cell<Admission> = const { std::cell::Cell::new(Admission::Checked) };
}

fn admission() -> Admission {
    ADMISSION.with(std::cell::Cell::get)
}

/// Switches this thread's decodes to [`Admission::Skipped`] until dropped, then restores whatever
/// was in force before, so a replayed field nested in a validating decode does not leave its
/// siblings unchecked, and a panic mid-decode does not leave the thread unchecked.
struct ReplayGuard {
    previous: Admission,
}

impl ReplayGuard {
    fn enter() -> Self {
        Self {
            previous: ADMISSION.with(|cell| cell.replace(Admission::Skipped)),
        }
    }
}

impl Drop for ReplayGuard {
    fn drop(&mut self) {
        ADMISSION.with(|cell| cell.set(self.previous));
    }
}

/// Deserialize with the engine's admission checks switched off, for an embedder replaying configs an
/// engine already admitted, from its own log or tables or from a peer (issue #1267). The skipped
/// checks are exactly those [`admission_check`] runs on the decoded value: a stub with no responses,
/// predicate `jsonpath`/`xpath` selectors and `matches` patterns, behaviors blocks that do not parse,
/// and `copy`/`lookup` selectors. (`admission_check` also runs the imposter-level binary-mode
/// check, #1578, which no decode runs.) Normalisation (aliases such as `rules`, `delayRange` →
/// `wait`, defaults) is unchanged.
///
/// Every other decode-time refusal still applies and is part of the stored format (issue #1268):
/// type and shape errors, an inverted `wait`/`delayRange` range, a `_behaviors` array or scalar
/// `behaviors`, a single-valued header named twice, a `_rift.fault.tcp` outside its bounds.
///
/// **The replay floor.** The floor is the first release that ships this function. A config an
/// engine at or after the floor admitted through any door decodes with `deserialize_replayed` on
/// every later engine, to the same value, and passes [`admission_check`] unless a check was added
/// after it was admitted. `tests/replay_floor.rs` holds the refusals that may not change.
///
/// A value decoded this way must pass [`admission_check`] before it is handed to
/// `ImposterManager`. `T` is generic, so an embedder's own type that contains `Stub`s decodes through
/// the same call, and it composes with `#[serde(deserialize_with = "deserialize_replayed")]`. The
/// switch is per thread and lasts for this call only; decoding is synchronous, so it never spans an
/// `.await`.
pub fn deserialize_replayed<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    let _replaying = ReplayGuard::enter();
    T::deserialize(deserializer)
}

/// The admission checks a config door runs while decoding an imposter, as a function (issue #1267):
/// every stub has at least one response (#1372), every predicate `jsonpath`/`xpath` selector and
/// `matches` pattern, every behaviors block parses, and every `copy`/`lookup` selector. Always checks, including when called from inside
/// [`deserialize_replayed`].
///
/// Its contract with the doors (issue #1268): a config a door admits passes; otherwise `Err` carries
/// the message of the first failure in the order a door decodes, so it names what the door would
/// have named. It judges the compiled program, which differs from what the door checked in one
/// place, a top-level `repeat`: the door checks the behaviors block before it, and merges it after.
/// So a malformed `repeat` inside the block that the top-level one replaces is refused only by the
/// door, and when the top-level `repeat` is itself malformed, both refuse but this names it first.
/// A response with more than one type, or none (#1551), is door-only for the same reason: the
/// decoded response holds exactly one type, so what the door refused cannot reach this function.
/// So are the behaviors rules #1582 added: a `behaviors` element with two behavior types or none,
/// a `0` or `null` `repeat`, and a `copy.from` or lookup `key.from` without exactly one key. The
/// compiled program no longer holds some of them, and engines up to v0.23.0 stored all of them, so
/// a store that replays (rift-cluster's log, `--datadir`) must not refuse them. The door checks
/// them after everything this function re-runs, so where both refuse they name the same failure.
///
/// Then the imposter-level binary-mode check (#1578), after every stub's, because the door decodes
/// every stub before it runs: see [`check_binary_mode`]. A replayed config never carries `mode`
/// (it is never serialized), so this part cannot newly refuse a stored config.
pub fn admission_check(config: &ImposterConfig) -> Result<(), String> {
    config.stubs.iter().try_for_each(admission_check_stub)?;
    check_binary_mode(config)
}

/// Mountebank's binary-mode refusal (issue #1578): an imposter created with `"mode": "binary"`
/// refuses every `matches` predicate and every `jsonpath`/`xpath` selector, with Mountebank's
/// messages. The gate is exactly `"binary"`, as Mountebank's `request.mode === 'binary'` is.
///
/// Mountebank raises these from a dry run of each stub against a synthetic request, so an `and`
/// whose earlier branch is false there, or an `or` whose earlier branch is true, never reaches a
/// later `matches`. Rift refuses statically, wherever the predicate sits: such a stub is dead weight
/// either way (docs/mountebank/differences.md).
///
/// An imposter-level admission check, never a decode refusal: it needs the imposter's `mode`, which
/// the create and apply doors read and then clear, so it is never stored or replayed.
pub(crate) fn check_binary_mode(config: &ImposterConfig) -> Result<(), String> {
    if config.mode.as_deref() != Some("binary") {
        return Ok(());
    }
    config
        .stubs
        .iter()
        .try_for_each(|stub| refuse_binary_mode_selectors(&stub.predicates))
}

/// The per-predicate half of [`check_binary_mode`], walking `not`/`and`/`or`. The selector is
/// checked before the operation, the order Mountebank's `normalize` and `matches` throw in.
fn refuse_binary_mode_selectors(predicates: &[Predicate]) -> Result<(), String> {
    predicates.iter().try_for_each(|predicate| {
        let selector = match &predicate.parameters.selector {
            Some(PredicateSelector::JsonPath { .. }) => Some("jsonpath"),
            Some(PredicateSelector::XPath { .. }) => Some("xpath"),
            None => None,
        };
        if let Some(selector) = selector {
            return Err(format!(
                "the {selector} predicate parameter is not allowed in binary mode"
            ));
        }
        match &predicate.operation {
            PredicateOperation::Matches(_) => {
                Err("the matches predicate is not allowed in binary mode".to_string())
            }
            PredicateOperation::Not(inner) => {
                refuse_binary_mode_selectors(std::slice::from_ref(&**inner))
            }
            PredicateOperation::And(inner) | PredicateOperation::Or(inner) => {
                refuse_binary_mode_selectors(inner)
            }
            _ => Ok(()),
        }
    })
}

/// [`admission_check`] for one stub. Responses first: they decode before `TryFrom<StubRaw>` checks
/// that there is at least one and then the stub's predicates, so this is the order a door reports in.
pub fn admission_check_stub(stub: &Stub) -> Result<(), String> {
    stub.responses.iter().try_for_each(|response| {
        refuse_unparseable_behaviors(response.behaviors_block(), Admission::Checked)?;
        refuse_bad_conditional(response.rift_block(), Admission::Checked)?;
        match response {
            StubResponse::Proxy { proxy, .. } => {
                refuse_unknown_proxy_mode(Some(proxy), Admission::Checked)?;
                refuse_unreadable_generators(Some(proxy), Admission::Checked)?;
                refuse_proxy_to_path(Some(proxy), Admission::Checked)?;
                refuse_bad_proxy_identity(Some(proxy), Admission::Checked)
            }
            _ => Ok(()),
        }
    })?;
    refuse_empty_responses(&stub.responses)?;
    validate_predicates(&stub.predicates)
}

/// A stub must carry at least one response, worded as Mountebank's `errorsForStub` words it (issue
/// #1372). An admission check, not a decode refusal: engines up to v0.21.0 admitted an absent or
/// empty `responses` on every door and served the imposter's `defaultResponse` when it matched.
/// An explicit `"responses": null` is a different, structural refusal that serde makes on its own.
fn refuse_empty_responses(responses: &[StubResponse]) -> Result<(), String> {
    if responses.is_empty() {
        return Err("'responses' must be a non-empty array".to_string());
    }
    Ok(())
}

/// Refuse a predicate the matcher could never evaluate as written, at parse, on every config door
/// (`POST`/`PUT /imposters`, the stub routes, `--configfile`/`--datadir`, `POST /admin/reload`).
///
/// Adding a refusal anywhere a config decodes changes the stored format an embedder replays (issue
/// #1268). A new check on anything reachable from `ImposterConfig` runs only under
/// [`Admission::Checked`] and is re-run by [`admission_check_stub`] on the decoded value. It may be
/// unconditional only when no engine ever admitted the input, which in practice means the shape
/// rules of a field introduced in the same change. `tests/replay_floor.rs` fails until the new
/// refusal is classified.
///
/// A `jsonpath`/`xpath` selector that does not compile used to be accepted and then read as the
/// empty string on every request, so `{"equals":{"body":""}}` behind it matched everything (issues
/// #181 and #1220). A `matches` pattern that does not compile was accepted and then never matched,
/// so the stub was silently dead (issue #1221). Walks `not`/`and`/`or` so a nested predicate is held
/// to the same rules.
pub(crate) fn validate_predicates(predicates: &[Predicate]) -> Result<(), String> {
    predicates.iter().try_for_each(validate_predicate)
}

fn validate_predicate(predicate: &Predicate) -> Result<(), String> {
    use crate::behaviors::{validate_jsonpath_selector, validate_xpath_selector};

    match &predicate.parameters.selector {
        Some(PredicateSelector::JsonPath { selector }) => {
            validate_jsonpath_selector(selector).map_err(|e| {
                format!("predicate `jsonpath` selector `{selector}` is invalid: {e}")
            })?;
            // Unless keys are case sensitive the matcher runs the lowercased selector (issue
            // #1601). As for XPath, one that compiles only as written is warned about, not refused.
            let case_sensitive = predicate.parameters.case_sensitive.unwrap_or(false);
            let folds = !predicate
                .parameters
                .key_case_sensitive
                .unwrap_or(case_sensitive);
            if folds
                && let Some(e) =
                    validate_jsonpath_selector(&crate::behaviors::fold_selector(selector)).err()
            {
                tracing::warn!(
                    "predicate `jsonpath` selector `{selector}` does not compile once lowercased \
                     for a key-case-insensitive match ({e}); it selects nothing"
                );
            }
        }
        Some(PredicateSelector::XPath { selector, .. }) => {
            validate_xpath_selector(selector)
                .map_err(|e| format!("predicate `xpath` selector `{selector}` is invalid: {e}"))?;
            // A case-insensitive predicate runs the lowercased selector (issue #1478). One that
            // compiles only as written is warned about, not refused: refusing it would make a
            // stored config that an engine already admitted undecodable.
            let folds = !predicate.parameters.case_sensitive.unwrap_or(false)
                && crate::imposter::predicates::folds_xpath_names(&predicate.operation);
            if folds
                && let Some(e) =
                    validate_xpath_selector(&crate::behaviors::fold_selector(selector)).err()
            {
                tracing::warn!(
                    "predicate `xpath` selector `{selector}` does not compile once lowercased for \
                     a case-insensitive match ({e}); it selects nothing"
                );
            }
        }
        None => {}
    }
    match &predicate.operation {
        PredicateOperation::Matches(fields) => {
            // The same case class the matcher compiles with (`fields.rs`): insensitive by default.
            let case_insensitive = !predicate.parameters.case_sensitive.unwrap_or(false);
            fields.iter().try_for_each(|(field, expected)| {
                validate_regex_leaves(expected, case_insensitive).map_err(|(pattern, e)| {
                    format!(
                        "predicate `matches` field `{field}` has an invalid regex `{pattern}`: {e}"
                    )
                })
            })
        }
        PredicateOperation::Not(inner) => validate_predicate(inner),
        PredicateOperation::And(inner) | PredicateOperation::Or(inner) => {
            validate_predicates(inner)
        }
        _ => Ok(()),
    }
}

/// Every string under a `matches` field is a pattern the matcher compiles: the field's own value
/// (`path`), a keyed value (`query.q`, `headers.x`), or any leaf of a JSON `body` object. A non-string
/// scalar is rendered to its JSON text, which is always a valid pattern. Returns the failing pattern
/// with its error.
fn validate_regex_leaves(
    value: &serde_json::Value,
    case_insensitive: bool,
) -> Result<(), (&str, regex::Error)> {
    use crate::imposter::predicates::regex_cache::validate_regex;

    match value {
        serde_json::Value::String(pattern) => {
            validate_regex(pattern, case_insensitive).map_err(|e| (pattern.as_str(), e))
        }
        serde_json::Value::Object(map) => map
            .values()
            .try_for_each(|v| validate_regex_leaves(v, case_insensitive)),
        serde_json::Value::Array(items) => items
            .iter()
            .try_for_each(|v| validate_regex_leaves(v, case_insensitive)),
        _ => Ok(()),
    }
}

/// Build a wait value from a delayRange array (uses first entry).
/// Emits a fixed value when min == max, otherwise a range object.
fn build_wait_from_delay_range(ranges: &[DelayRange]) -> serde_json::Value {
    let first = &ranges[0];
    if first.min == first.max {
        serde_json::Value::Number(first.min.into())
    } else {
        serde_json::json!({ "min": first.min, "max": first.max })
    }
}

/// Inject a `wait` value into a stub response's `_behaviors`, but only when
/// the response does not already have an explicit wait configured.
fn inject_wait_behavior(response: StubResponse, wait_val: serde_json::Value) -> StubResponse {
    match response {
        StubResponse::Is {
            is,
            behaviors,
            rift,
            ..
        } => {
            // `behaviors` is a compiled program (`new_is` compiled it), where a `null` wait is
            // already gone (issue #1093) — so "no wait step" is exactly "no explicit wait".
            let mut program = match behaviors {
                Some(serde_json::Value::Array(program)) => program,
                _ => Vec::new(),
            };
            let has_wait = program
                .iter()
                .any(|element| program_element(element).is_some_and(|(k, _)| k == "wait"));
            if !has_wait {
                // The stub's delay runs first, where an object-form `wait` would.
                let at = usize::from(
                    program
                        .first()
                        .and_then(program_element)
                        .is_some_and(|(k, _)| k == "repeat"),
                );
                program.insert(at, serde_json::json!({ "wait": wait_val }));
            }
            let behaviors = Some(serde_json::Value::Array(program));
            // Behaviors changed (the `wait` entry was injected) — recompute via `new_is` rather
            // than reusing the stale `behaviors_parsed`/`rendered_body` that were dropped above.
            StubResponse::new_is(is, behaviors, rift)
        }
        other => other,
    }
}

// Predicate types live in the shared `rift-types` crate (issue #36) so the proxy and the
// linter share one definition. Re-exported here so existing `crate::imposter::types::*`
// paths keep resolving unchanged.
pub use rift_types::{Predicate, PredicateOperation, PredicateParameters, PredicateSelector};

/// The response-type keys, in the precedence `TryFrom<StubResponseRaw>` reads them. A config door
/// refuses a response with two (#1551), so the precedence only decides what a replayed config an
/// older engine admitted decodes to. Pinned to that conversion
/// by `response_variant_keys_follow_the_decode_precedence`; the JSON Schema carries it as
/// `x-rift-response-variants` (issue #1342).
#[cfg(feature = "schema")]
pub(crate) const RESPONSE_VARIANT_KEYS: [&str; 4] = ["is", "proxy", "inject", "fault"];

/// Response within a stub - wrapper type that handles various formats
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "StubResponseRaw", into = "StubResponseOut")]
pub enum StubResponse {
    Is {
        is: IsResponse,
        #[serde(rename = "_behaviors", skip_serializing_if = "Option::is_none")]
        behaviors: Option<serde_json::Value>,
        /// Boxed (issue #1209): an enum is as large as its largest variant, so an inline extension
        /// here is charged to every response — including the plain `is` responses that never carry
        /// one. The deref this adds is off the fast path: a response with no `_rift` effect is
        /// served from `prepared` without touching this field, and one that does carry a block is
        /// on the slow path already, where the read sits in front of a fault sleep, a template
        /// render or a script evaluation.
        #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
        rift: Option<Box<RiftResponseExtension>>,
        /// Issue #479: `_behaviors` parsed ONCE at construction so the request hot path never
        /// re-deserializes it. Not serde-driven (see the `from`/`into` attributes on this enum) —
        /// purely a precomputed cache alongside `behaviors`.
        #[serde(skip)]
        behaviors_parsed: Option<std::sync::Arc<crate::behaviors::BehaviorProgram>>,
        /// Issue #479: pre-rendered JSON string for a non-string `is.body`, computed ONCE at
        /// construction so the request hot path never re-serializes it. `None` for a string body
        /// (served as-is) or no body at all.
        #[serde(skip)]
        rendered_body: Option<std::sync::Arc<str>>,
        /// Issue #703: fully-materialized static response (status + `HeaderMap` + `Bytes`), computed
        /// ONCE at construction so the hot path serves it as refcount clones. `Some` only for a
        /// request-independent `is` response (no templates/date tokens/`_behaviors`/`_rift` effect,
        /// text mode, valid headers); `None` routes to the existing slow path. Derived cache, like
        /// the two fields above — not serde-driven.
        #[serde(skip)]
        prepared: Option<std::sync::Arc<crate::imposter::response::PreparedResponse>>,
    },
    /// Built only through [`StubResponse::new_proxy`], which keeps `behaviors_parsed` in step with
    /// `behaviors`.
    Proxy {
        /// Boxed (issue #1609), for the reason `ignored_rift` is: `ProxyResponse` was this enum's
        /// largest payload, so its size was charged to every response, and the client identity
        /// (`cert`/`key`/`passphrase`) would have grown it again. Proxy responses are rare; the
        /// deref sits in front of an upstream round trip.
        proxy: Box<ProxyResponse>,
        /// A `_rift` block written on this response. No `_rift` feature applies to a `proxy` response, so
        /// it is kept only to be reported (`config_key_ignored`, issue #1152) and to round-trip.
        ///
        /// Boxed (issue #1206): an enum is as large as its largest variant, so carrying the
        /// extension by value here sized *every* response — including the plain `is` responses that
        /// never have one — at 720 bytes. The admin create path materializes each stub several
        /// times over, so that landed as ~3 MB of RSS per 4,000 stubs. The `Inject` and `Fault`
        /// variants below carry it for the same reason.
        ignored_rift: Option<Box<RiftResponseExtension>>,
        /// The behaviors block, run on the upstream response before it is recorded, as Mountebank
        /// does (issue #1189). `None` for an absent or empty block.
        behaviors: Option<serde_json::Value>,
        /// `behaviors`, parsed once at construction (issue #479).
        behaviors_parsed: Option<std::sync::Arc<crate::behaviors::BehaviorProgram>>,
    },
    /// Built only through [`StubResponse::new_inject`], which keeps `behaviors_parsed` in step
    /// with `behaviors`.
    Inject {
        inject: String,
        /// A `_rift` block written on this response. No `_rift` feature applies to a `inject` response, so
        /// it is kept only to be reported (`config_key_ignored`, issue #1152) and to round-trip.
        ignored_rift: Option<Box<RiftResponseExtension>>,
        /// The behaviors block, run on the injected response as Mountebank does (issue #1188).
        /// `None` for an absent or empty block.
        behaviors: Option<serde_json::Value>,
        /// `behaviors`, parsed once at construction (issue #479).
        behaviors_parsed: Option<std::sync::Arc<crate::behaviors::BehaviorProgram>>,
    },
    Fault {
        fault: String,
        /// A `_rift` block written on this response. No `_rift` feature applies to a `fault` response, so
        /// it is kept only to be reported (`config_key_ignored`, issue #1152) and to round-trip.
        ignored_rift: Option<Box<RiftResponseExtension>>,
        /// A behaviors block written on this response. Only its `repeat` takes effect on a `fault`
        /// response (issue #1188); the rest is kept only to be reported (`config_key_ignored`, issue
        /// #1181), to round-trip, and for the `--allowInjection` gate to classify. `None` for an
        /// absent or empty block.
        ignored_behaviors: Option<serde_json::Value>,
    },
    /// Rift script-only response (no `is` block, response generated by script)
    RiftScript {
        /// Boxed for the same reason as `Is.rift` (issue #1209) — see the note there.
        rift: Box<RiftResponseExtension>,
        /// A behaviors block written beside a `_rift`-only response. Only its `repeat` takes effect on
        /// a script response (issue #1188); the rest is kept only to be reported
        /// (`config_key_ignored`, issue #1181), to round-trip, and for the `--allowInjection` gate to
        /// classify. `None` for an absent or empty block.
        ignored_behaviors: Option<serde_json::Value>,
    },
}

impl StubResponse {
    /// Build an `Is` response, precomputing (issue #479) the parsed `_behaviors` and — for a
    /// non-string body only — the rendered JSON string, so the request hot path never
    /// re-deserializes/re-serializes them on every request. This is the ONLY place an `Is`
    /// variant should be constructed; callers that need to rebuild one after mutating `behaviors`
    /// (e.g. `inject_wait_behavior`) must go through here too, to keep the caches consistent.
    pub(crate) fn new_is(
        is: IsResponse,
        behaviors: Option<serde_json::Value>,
        rift: Option<Box<RiftResponseExtension>>,
    ) -> StubResponse {
        let behaviors = behaviors.and_then(compile_behaviors);
        let behaviors_parsed = parse_behaviors(behaviors.as_ref());
        // Only a non-string body needs rendering; a string body is served as-is. Serializing a
        // `serde_json::Value` is infallible by construction, so this never defaults (issue #611).
        let rendered_body =
            is.body.as_ref().filter(|b| !b.is_string()).map(|b| {
                std::sync::Arc::from(serde_json::to_string(b).unwrap_or_default().as_str())
            });
        // Issue #703: materialize the static-serving form once, here (same pass, same invalidation
        // as `rendered_body`). `None` for any request-dependent response — the slow path covers it.
        let prepared = crate::imposter::response::PreparedResponse::try_build(
            &is,
            rendered_body.as_ref(),
            rift.as_deref(),
            behaviors_parsed.is_some(),
        )
        .map(std::sync::Arc::new);
        StubResponse::Is {
            is,
            behaviors,
            rift,
            behaviors_parsed,
            rendered_body,
            prepared,
        }
    }

    /// Build a `Proxy` response, parsing its behaviors block once (issue #479). The only place a
    /// `Proxy` variant should be constructed.
    pub(crate) fn new_proxy(
        proxy: ProxyResponse,
        ignored_rift: Option<Box<RiftResponseExtension>>,
        behaviors: Option<serde_json::Value>,
    ) -> StubResponse {
        let behaviors = behaviors.and_then(compile_behaviors);
        let behaviors_parsed = parse_behaviors(behaviors.as_ref());
        StubResponse::Proxy {
            proxy: Box::new(proxy),
            ignored_rift,
            behaviors,
            behaviors_parsed,
        }
    }

    /// Build an `Inject` response, parsing its behaviors block once (issue #479). The only place an
    /// `Inject` variant should be constructed.
    pub(crate) fn new_inject(
        inject: String,
        ignored_rift: Option<Box<RiftResponseExtension>>,
        behaviors: Option<serde_json::Value>,
    ) -> StubResponse {
        let behaviors = behaviors.and_then(compile_behaviors);
        let behaviors_parsed = parse_behaviors(behaviors.as_ref());
        StubResponse::Inject {
            inject,
            ignored_rift,
            behaviors,
            behaviors_parsed,
        }
    }

    /// Whether a proxy recorded this response (issue #1519). Only an `is` response carries the mark;
    /// Mountebank reads only `response.is`, so the flat recorded form (#304) is never one.
    #[must_use]
    pub fn is_recorded(&self) -> bool {
        matches!(self, StubResponse::Is { is, .. } if is.is_recorded())
    }

    /// The `_rift` block written on this response, whatever its type.
    pub(crate) fn rift_block(&self) -> Option<&RiftResponseExtension> {
        match self {
            StubResponse::Is { rift, .. } => rift.as_deref(),
            StubResponse::RiftScript { rift, .. } => Some(rift),
            StubResponse::Proxy { ignored_rift, .. }
            | StubResponse::Inject { ignored_rift, .. }
            | StubResponse::Fault { ignored_rift, .. } => ignored_rift.as_deref(),
        }
    }

    /// The compiled behaviors program on this response, whatever its type — also where `repeat` is
    /// read from (issues #1188, #1198).
    pub(crate) fn behaviors_block(&self) -> Option<&serde_json::Value> {
        match self {
            StubResponse::Is { behaviors, .. }
            | StubResponse::Inject { behaviors, .. }
            | StubResponse::Proxy { behaviors, .. } => behaviors.as_ref(),
            StubResponse::Fault {
                ignored_behaviors, ..
            }
            | StubResponse::RiftScript {
                ignored_behaviors, ..
            } => ignored_behaviors.as_ref(),
        }
    }
}

/// Parse a compiled behaviors program for the serve path, once (issue #479).
///
/// A program that won't parse can't be cached, so it is dropped — but never silently (issue #608).
/// No config door reaches this since #1162: the parser refuses such a block
/// (`refuse_unparseable_behaviors`). It stays for programmatic callers, which can pass anything.
fn parse_behaviors(
    behaviors: Option<&serde_json::Value>,
) -> Option<std::sync::Arc<crate::behaviors::BehaviorProgram>> {
    let program = behaviors?;
    // Only a compiled program is parsed; every constructor compiles first, so this is a
    // programmatically built value that `compile_behaviors` could not read (issue #1101 is why a
    // bare array is never read positionally).
    let Some(elements) = program.as_array() else {
        tracing::error!(
            behaviors = %program,
            "behaviors block is not a compiled program and will be ignored if this response is served"
        );
        return None;
    };
    match crate::behaviors::BehaviorProgram::from_elements(elements) {
        Ok(parsed) => Some(std::sync::Arc::new(parsed)),
        Err(e) => {
            // Deliberately does not claim the response will serve: construction runs before the
            // admin gate, so this config may still be rejected.
            tracing::error!(
                error = %e,
                behaviors = %program,
                "malformed behaviors block could not be parsed and will be ignored if this \
                 response is served"
            );
            None
        }
    }
}

/// Raw deserialization type that handles multiple JSON formats for stub responses
/// Supports:
/// - Standard Mountebank format with `is`, `proxy`, `inject`, or `fault` fields
/// - Formats with `behaviors` (without underscore) or `_behaviors`
/// - Formats with `proxy: null` alongside `is` (ignored)
/// - `statusCode` as either string or number
/// - Rift extensions via `_rift` field
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        description = "One response of a stub: an `is`, `proxy`, `inject` or `fault` response, a `_rift`-only script response, or the flat recorded form (`statusCode`/`headers`/`body` at the top level). Exactly one of `is`, `proxy`, `inject`, `fault`, or a `_rift`-only or flat response; a config door refuses two, or none."
    )
)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StubResponseRaw {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is: Option<IsResponseRaw>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
    /// Mountebank-style behaviors (with underscore prefix): an object, or absent/`null`.
    #[serde(
        rename = "_behaviors",
        default,
        deserialize_with = "deserialize_underscore_behaviors"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<crate::behaviors::ResponseBehaviors>")
    )]
    pub underscore_behaviors: Option<serde_json::Value>,
    /// Alternative behaviors field (without underscore, used by some tools): an object or an
    /// array of behavior objects, or absent/`null`.
    #[serde(default, deserialize_with = "deserialize_behaviors")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::behaviors_block")
    )]
    pub behaviors: Option<serde_json::Value>,
    /// Rift extensions for advanced features
    #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
    pub rift: Option<RiftResponseExtension>,
    /// Flat / recorded response form (issue #304): `statusCode`/`headers`/`body`/`_mode` at the
    /// top level with no `is` wrapper (the shape emitted by recorded/migrated mocks). Mountebank
    /// renders these exactly like `is: { … }`. `is` still takes precedence when both are present.
    #[serde(default, deserialize_with = "optional_status_value")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<crate::schema::ResponseStatusCode>")
    )]
    pub status_code: Option<StatusValue>,
    #[serde(default, deserialize_with = "present_headers")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::MultiValueHeaders")
    )]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_headers"))]
    pub headers: Option<Headers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(rename = "_mode", default, deserialize_with = "present_mode")]
    #[cfg_attr(feature = "schema", schemars(with = "ResponseMode"))]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_mode"))]
    pub mode: Option<ResponseMode>,
    /// Mountebank's canonical `repeat` (issue #1188): it lifts `_behaviors.repeat` to the response,
    /// so this is the form `mb save` writes. Merged into the behaviors block at parse, where it
    /// wins over a `repeat` inside the block, as it does in Mountebank. Must be greater than 0: a
    /// config door refuses `0` and `null`, as Mountebank does (issue #1582).
    // Decoded with its presence, so a written `null` is `Some(Null)` for the door to refuse; a
    // replayed decode reads it as absent, as every engine before #1582 did.
    #[serde(default, deserialize_with = "present_value")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<u32>", range(max = 4_294_967_295_u64))
    )]
    pub repeat: Option<serde_json::Value>,
}

/// Serialization type for stub responses - outputs Mountebank-compatible format
/// Uses `behaviors` as array (Mountebank standard format)
/// Field ordering matches Mountebank: behaviors, is, proxy
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StubResponseOut {
    /// Mountebank's response-level `repeat` (issue #1191): Mountebank reads it only here, and
    /// refuses a `{"repeat": n}` element in `behaviors`, as a config door does since #1582.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<serde_json::Value>,
    /// Mountebank-style behaviors as array (standard Mountebank output format)
    /// Placed first to match Mountebank output ordering
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behaviors: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is: Option<IsResponseOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
    /// Rift extensions for advanced features
    #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
    pub rift: Option<RiftResponseExtension>,
}

/// What a response's author left unwritten. Set only where JSON is decoded, and so
/// `Omitted::default()` ("nothing omitted") is what every response built in code carries: a
/// recorded stub is complete as it stands and must never pick up a default (issue #1369).
///
/// This is the one presence representation: the decoders record it, the printers honour it, and
/// [`IsResponse::completed_by`] reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Omitted {
    pub status_code: bool,
    pub headers: bool,
    pub mode: bool,
    pub body: bool,
}

/// A response's `statusCode` as written (issue #1558): a status, or a string Mountebank keeps as
/// written and reads only when it writes the response — a `copy`/`lookup` token, say, which the
/// behaviors substitute first.
///
/// A string that parses as a `u16` is a [`Code`](Self::Code), so it prints as the number it spells
/// (#1361) and every stub admitted before keys as it did (#1256). Only the imposter grammar reads
/// this; the intercept rules keep [`rift_types::wire::deserialize_status_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusValue {
    Code(u16),
    Text(Box<str>),
}

impl StatusValue {
    /// A string `statusCode`: a [`Code`](Self::Code) when it parses as a `u16`, else kept as
    /// written. The one rule for the grammar and for a status an `inject` returns (issue #1635).
    pub(crate) fn from_text(text: String) -> Self {
        match text.parse::<u16>() {
            Ok(code) => StatusValue::Code(code),
            Err(_) => StatusValue::Text(text.into_boxed_str()),
        }
    }
}

impl Serialize for StatusValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            StatusValue::Code(code) => serializer.serialize_u16(*code),
            StatusValue::Text(text) => serializer.serialize_str(text),
        }
    }
}

/// [`StatusValue`] from JSON: any string, or what [`rift_types::wire::deserialize_status_code`]
/// reads otherwise — a number within `u16` — with its refusals (a boolean, `null`, an object, an
/// out-of-range number) and their messages unchanged.
fn status_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<StatusValue, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(text) => Ok(StatusValue::from_text(text)),
        other => rift_types::wire::deserialize_status_code(other)
            .map(StatusValue::Code)
            .map_err(serde::de::Error::custom),
    }
}

/// `Some(..)` for a field that is present. Serde calls a `deserialize_with` function only for a
/// present field, so an absent one stays `None` through `#[serde(default)]`, and a present `null`
/// is judged by the inner decoder exactly as it was before presence was recorded.
fn present_status_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<StatusValue>, D::Error> {
    status_value(d).map(Some)
}

/// The flat form's `statusCode` (issue #304): a `null` is absent, as before.
fn optional_status_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<StatusValue>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        value => status_value(value)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn present_headers<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Headers>, D::Error> {
    multi_value_headers::deserialize(d).map(Some)
}

fn present_mode<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<ResponseMode>, D::Error> {
    ResponseMode::deserialize(d).map(Some)
}

// The published schema documents what an omitted field means. These are read only by
// `schemars(default = ...)`: at runtime an omitted field stays `None` and is recorded (#1369).
#[cfg(feature = "schema")]
fn schema_default_status_code() -> Option<StatusValue> {
    Some(StatusValue::Code(default_status_code()))
}

#[cfg(feature = "schema")]
fn schema_default_headers() -> Option<Headers> {
    Some(Headers::new())
}

#[cfg(feature = "schema")]
fn schema_default_mode() -> Option<ResponseMode> {
    Some(ResponseMode::Text)
}

/// Raw IsResponse that handles statusCode as string or number (for deserialization)
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(rename = "IsResponse", description = "A canned response.")
)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IsResponseRaw {
    #[serde(default, deserialize_with = "present_status_value")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "crate::schema::ResponseStatusCode")
    )]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_status_code"))]
    pub status_code: Option<StatusValue>,
    #[serde(default, deserialize_with = "present_headers")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::MultiValueHeaders")
    )]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_headers"))]
    pub headers: Option<Headers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    #[serde(rename = "_mode", default, deserialize_with = "present_mode")]
    #[cfg_attr(feature = "schema", schemars(with = "ResponseMode"))]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_mode"))]
    pub mode: Option<ResponseMode>,
    /// The proxy round trip in milliseconds, written on each response a proxy records, as
    /// Mountebank writes it. A number marks a recorded response, which
    /// `DELETE /imposters/:port/savedProxyResponses` removes; any other value is kept as written.
    // Any JSON value decodes: refusing one would make stored configs undecodable (rift-cluster
    // replays them), and Mountebank only tests `typeof === 'number'`.
    #[serde(
        rename = "_proxyResponseTime",
        default,
        deserialize_with = "present_value"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "Option<serde_json::Value>"))]
    pub proxy_response_time: Option<serde_json::Value>,
}

impl From<IsResponseRaw> for IsResponse {
    fn from(raw: IsResponseRaw) -> Self {
        IsResponse {
            proxy_response_time: raw.proxy_response_time,
            ..IsResponse::decoded(raw.status_code, raw.headers, raw.body, raw.mode)
        }
    }
}

/// IsResponse for serialization. A field the author omitted is `None` and not written, so the
/// printed form reads as it was written.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IsResponseOut {
    /// A JSON number (issue #1361), as `defaultResponse.statusCode` and Mountebank's own
    /// recordings print it; a posted `"202"` therefore prints as `202`. A string that is not a
    /// number prints as written (issue #1558).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<StatusValue>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_present_headers"
    )]
    pub headers: Option<Headers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    /// Written whenever the response wrote it or was built in code (a recording prints
    /// `"_mode": "text"`, as Mountebank's recorder does); an omitted one stays omitted so a
    /// `defaultResponse` can still supply it.
    #[serde(rename = "_mode", skip_serializing_if = "Option::is_none")]
    pub mode: Option<ResponseMode>,
    /// See [`IsResponse::proxy_response_time`]. Printed everywhere but the replayable form.
    #[serde(rename = "_proxyResponseTime", skip_serializing_if = "Option::is_none")]
    pub proxy_response_time: Option<serde_json::Value>,
}

impl From<IsResponse> for IsResponseOut {
    fn from(is: IsResponse) -> Self {
        IsResponseOut {
            status_code: (!is.omitted.status_code).then(|| is.status_value()),
            headers: (!is.omitted.headers).then_some(is.headers),
            body: is.body,
            mode: (!is.omitted.mode).then_some(is.mode),
            proxy_response_time: is.proxy_response_time,
        }
    }
}

/// `Some(..)` for a present field, `null` included, so a written value prints back as written.
fn present_value<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(d).map(Some)
}

fn serialize_present_headers<H, S>(headers: &Option<H>, serializer: S) -> Result<S::Ok, S::Error>
where
    H: std::borrow::Borrow<Headers>,
    S: serde::Serializer,
{
    match headers {
        Some(headers) => multi_value_headers::serialize(headers.borrow(), serializer),
        None => serializer.serialize_none(),
    }
}

pub(crate) fn default_status_code() -> u16 {
    200
}

/// `_behaviors` must be an object (issue #1101). `ResponseBehaviors` is a derived named-field struct,
/// so serde would also read a JSON array into it by position — wait, repeat, copy, lookup,
/// shellTransform, decorate — and the `--allowInjection` gate, which classifies object keys, saw
/// nothing to refuse. Refusing the shape here means no door can admit it. `null` stays absent (#1098).
/// A `{min,max}` wait with `min > max` has no delay to draw from, and the draw used to panic the
/// request worker (issue #1148). Refused here, at the same layer as #1109's shape checks, so every
/// door reports it without per-door code. Looks only at the numeric range shape; every other `wait`
/// spelling keeps its existing policy.
fn refuse_inverted_wait(wait: &serde_json::Value) -> Result<(), String> {
    let (Some(min), Some(max)) = (
        wait.get("min").and_then(serde_json::Value::as_u64),
        wait.get("max").and_then(serde_json::Value::as_u64),
    ) else {
        return Ok(());
    };
    if min > max {
        return Err(format!("`wait` range has min {min} greater than max {max}"));
    }
    Ok(())
}

/// Every `wait` in a `behaviors` block, whichever spelling it takes. A later `null` can remove a
/// `wait` from the program that runs, but an inverted range is wrong wherever it sits, so all of
/// them are checked rather than only the ones that survive compilation.
fn refuse_inverted_waits_in_block(block: &serde_json::Value) -> Result<(), String> {
    match block {
        serde_json::Value::Object(_) => {
            if let Some(wait) = block.get("wait") {
                refuse_inverted_wait(wait)?;
            }
        }
        serde_json::Value::Array(entries) => {
            for entry in entries {
                if let Some(wait) = entry.get("wait") {
                    refuse_inverted_wait(wait)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// The block `new_is` is about to parse must parse (issue #1162). `new_is` cannot fail, so a block
/// it could not read used to be admitted and served with every behavior but `repeat` ignored and
/// only a log line to say so. Checked on the compiled program — every step that will run — so a
/// step a later `null` removes, or a `behaviors` shadowed by `_behaviors`, is not refused: it
/// configures nothing, and rift-lint reads the array the same way. Every other element is live
/// (#1195, #1198), so a malformed earlier element is refused rather than overridden.
///
/// An admission check, not a structural one (issue #1267): the block is stored as written, so a
/// replayed decode ([`Admission::Skipped`]) skips it and [`admission_check_stub`] runs it on the
/// decoded value with the same result. The decode passes the thread's mode; `admission_check_stub`
/// always passes [`Admission::Checked`].
fn refuse_unparseable_behaviors(
    block: Option<&serde_json::Value>,
    admission: Admission,
) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    match block {
        // A compiled program: every step runs, so every step must parse.
        Some(serde_json::Value::Array(program)) => program
            .iter()
            .try_for_each(|element| refuse_unparseable_behaviors(Some(element), admission)),
        Some(object @ serde_json::Value::Object(_)) => refuse_unparseable_object(object),
        // Nothing else reaches here: the field deserializers refuse other shapes.
        _ => Ok(()),
    }
}

/// A fixed `_rift.conditional.lastModified` must be an IMF-fixdate (issue #1280): one that does not
/// parse could never be compared with an `If-Modified-Since`, and is served verbatim as a header no
/// cache can read. An admission check (issue #1268), so a replayed decode skips it and
/// [`admission_check_stub`] runs it with the same message. Checked on every response type, like the
/// behaviors block, since the `_rift` block is read before the type is chosen.
fn refuse_bad_conditional(
    rift: Option<&RiftResponseExtension>,
    admission: Admission,
) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    let fixed = rift
        .and_then(|r| r.conditional.as_ref())
        .and_then(ConditionalGet::resolved)
        .and_then(|(_, last_modified)| match last_modified {
            LastModified::Fixed(date) => Some(date),
            LastModified::Load => None,
        });
    match fixed {
        Some(date) if crate::imposter::conditional::parse_imf_fixdate(date).is_none() => {
            Err(format!(
                "`_rift.conditional.lastModified` must be \"load\" or an HTTP-date such as \
             \"Sat, 03 Oct 2026 12:00:00 GMT\"; got \"{date}\""
            ))
        }
        _ => Ok(()),
    }
}

fn refuse_unparseable_object(block: &serde_json::Value) -> Result<(), String> {
    let Some(keys) = block.as_object() else {
        return Ok(());
    };
    if let Ok(behaviors) = crate::behaviors::ResponseBehaviors::deserialize(block) {
        validate_behavior_selectors(&behaviors)?;
        return refuse_bad_extraction_namespaces(block);
    }
    // serde's message names nothing the author wrote ("did not match any variant of untagged enum
    // WaitBehavior"), so find the key. The behaviors are independent fields, so one key parses or
    // fails on its own exactly as it does inside the block.
    for (key, value) in keys {
        let alone = serde_json::json!({ key.as_str(): value });
        let Err(e) = crate::behaviors::ResponseBehaviors::deserialize(&alone) else {
            continue;
        };
        return Err(if key == "wait" {
            format!(
                "`wait` must be a non-negative integer of milliseconds, a {{\"min\", \"max\"}} pair \
                 of non-negative integers, a function string or {{\"inject\": \"function() {{ ... }}\"}}; \
                 got {value}"
            )
        } else {
            format!("`{key}` behavior is malformed: {e}")
        });
    }
    // Every key parses alone, so the failure is in no single key; report the block as a whole
    // rather than admit it.
    Err("behaviors block is malformed".to_string())
}

/// Mountebank's message for a `repeat` it refuses (`dryRunValidator.js` `addRepeatErrorsTo`).
const REPEAT_NOT_POSITIVE: &str = "\"repeat\" field must be an integer greater than 0";

/// The `copy` or `lookup` items of a behaviors block or program element: one object, or a list.
fn behavior_items<'a>(block: &'a serde_json::Value, key: &str) -> Vec<&'a serde_json::Value> {
    match block.get(key) {
        Some(serde_json::Value::Array(items)) => items.iter().collect(),
        Some(item @ serde_json::Value::Object(_)) => vec![item],
        _ => Vec::new(),
    }
}

/// A `copy.from` or lookup `key.from` written as an object names exactly one request field, as
/// Mountebank's `singleKeyOnly` rule requires (issue #1582). `CopySource` decodes any map, and read
/// `query` before `headers` and ignored the rest, so a two-key `from` loaded and silently picked
/// one; an empty one extracted nothing. Checked on the JSON as written, so the map cannot hide the
/// count.
fn refuse_from_key_count(block: &serde_json::Value) -> Result<(), String> {
    let copies = behavior_items(block, "copy")
        .into_iter()
        .map(|copy| ("copy behavior \"from\"", copy.get("from")));
    let lookups = behavior_items(block, "lookup").into_iter().map(|lookup| {
        (
            "lookup behavior \"key.from\"",
            lookup.get("key").and_then(|key| key.get("from")),
        )
    });
    for (field, from) in copies.chain(lookups) {
        if let Some(serde_json::Value::Object(from)) = from
            && from.len() != 1
        {
            return Err(format!("{field} field must have exactly one key"));
        }
    }
    Ok(())
}

/// Refuse, at a config door, what Mountebank's `behaviorsValidator.js` and `dryRunValidator.js`
/// refuse in a behaviors block and engines up to v0.23.0 admitted (issue #1582), in the order
/// Mountebank reports them:
/// - a `copy.from` or lookup `key.from` object without exactly one key;
/// - in the `behaviors` array form, an element with no behavior key but some other key
///   (`Unrecognized behavior: "<key>"`, which covers a `{"repeat": n}` element: `repeat` is a
///   response field there), and an element with two or more behavior keys. An unknown key beside
///   one behavior is admitted, as there. The `_behaviors` object form is upcast key by key, so it
///   is never refused for either;
/// - a `repeat` of `0` or `null`, on the response or in an object-form block.
///
/// The first key named is the first in the element's sorted keys, which is the written order for
/// the one-key element this is almost always about.
///
/// **Door-only.** [`compile_behaviors`] splits a multi-key element into one step per key and drops
/// a `null` `repeat`, which is the stored program shape (#1198), so [`admission_check_stub`] cannot
/// see those. A `0` `repeat` and a `from` of the wrong size it could see, and does not check either:
/// engines up to v0.23.0 stored all of these (a `from` as written), and a store replays what it
/// holds, `--datadir` included, so a check there would make an upgrade drop a stored imposter. A
/// replayed decode ([`Admission::Skipped`]) keeps the value those engines stored.
fn refuse_door_only_behavior_shapes(
    raw: &StubResponseRaw,
    admission: Admission,
) -> Result<(), String> {
    use serde_json::Value;
    if admission == Admission::Skipped {
        return Ok(());
    }
    let not_positive = |repeat: Option<&Value>| {
        repeat.is_some_and(|repeat| repeat.is_null() || repeat.as_u64() == Some(0))
    };
    let known = |key: &String| crate::behaviors::CANONICAL_ORDER.contains(&key.as_str());
    match raw.underscore_behaviors.as_ref().or(raw.behaviors.as_ref()) {
        Some(Value::Array(elements)) => {
            for element in elements {
                refuse_from_key_count(element)?;
                let Some(element) = element.as_object() else {
                    continue;
                };
                let behaviors = element.keys().filter(|key| known(key)).count();
                if let (0, Some(first)) = (behaviors, element.keys().next()) {
                    return Err(format!("Unrecognized behavior: \"{first}\""));
                }
                if behaviors > 1 {
                    return Err("Each behavior object must have only one behavior type".to_string());
                }
            }
        }
        Some(block @ Value::Object(_)) => {
            refuse_from_key_count(block)?;
            if not_positive(block.get("repeat")) {
                return Err(REPEAT_NOT_POSITIVE.to_string());
            }
        }
        _ => {}
    }
    if not_positive(raw.repeat.as_ref()) {
        return Err(REPEAT_NOT_POSITIVE.to_string());
    }
    Ok(())
}

/// Every `copy` and `lookup` selector compiles (issue #1258). The extractors read a selector that
/// does not compile as "nothing extracted", so the token became the empty string (or the lookup
/// found no row) on every request, the behavior twin of the predicate refusal in
/// [`validate_predicate`] (#1220, #1221).
fn validate_behavior_selectors(
    behaviors: &crate::behaviors::ResponseBehaviors,
) -> Result<(), String> {
    let copies = behaviors.copy.iter().map(|copy| ("copy", &copy.extraction));
    let lookups = behaviors
        .lookup
        .iter()
        .map(|lookup| ("lookup", &lookup.key.extraction));
    copies
        .chain(lookups)
        .try_for_each(|(key, extraction)| validate_extraction_selector(key, extraction))
}

/// Every `copy`/`lookup` `using.ns` is an object of prefix → URI strings on an XPath `using`
/// (issue #1326). Read from the raw block: the decode reads a malformed `ns` as absent, because the
/// key was ignored before #1326 and stored configs may carry any shape there.
fn refuse_bad_extraction_namespaces(block: &serde_json::Value) -> Result<(), String> {
    let copies = behavior_items(block, "copy")
        .into_iter()
        .map(|copy| ("copy", copy.get("using")));
    let lookups = behavior_items(block, "lookup")
        .into_iter()
        .map(|lookup| ("lookup", lookup.get("key").and_then(|key| key.get("using"))));
    for (key, using) in copies.chain(lookups) {
        let Some(using) = using else { continue };
        let Some(ns) = using.get("ns").filter(|ns| !ns.is_null()) else {
            continue;
        };
        let method = using
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if method != "xpath" {
            return Err(format!(
                "`{key}` behavior `{method}` has `using.ns`, which only applies to `method: xpath`"
            ));
        }
        let well_formed = ns
            .as_object()
            .is_some_and(|map| map.values().all(serde_json::Value::is_string));
        if !well_formed {
            return Err(format!(
                "`{key}` behavior `xpath` `using.ns` must be an object mapping each prefix to a \
                 namespace URI string; got {ns}"
            ));
        }
    }
    Ok(())
}

fn validate_extraction_selector(
    key: &str,
    extraction: &crate::behaviors::ExtractionMethod,
) -> Result<(), String> {
    use crate::behaviors::{ExtractionMethod, validate_jsonpath_selector, validate_xpath_selector};
    use crate::imposter::predicates::regex_cache::validate_regex;

    let (method, selector, compiled) = match extraction {
        ExtractionMethod::Regex { selector, options } => {
            let case_insensitive = options.as_ref().is_some_and(|o| o.ignore_case);
            let compiled = validate_regex(selector, case_insensitive).map_err(|e| e.to_string());
            ("regex", selector, compiled)
        }
        ExtractionMethod::JsonPath { selector } => {
            ("jsonpath", selector, validate_jsonpath_selector(selector))
        }
        ExtractionMethod::XPath { selector, .. } => {
            ("xpath", selector, validate_xpath_selector(selector))
        }
    };
    compiled
        .map_err(|e| format!("`{key}` behavior `{method}` selector `{selector}` is invalid: {e}"))
}

fn deserialize_underscore_behaviors<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        block @ (None | Some(serde_json::Value::Object(_))) => {
            if let Some(inner) = &block {
                refuse_inverted_waits_in_block(inner).map_err(serde::de::Error::custom)?;
            }
            Ok(block)
        }
        Some(other) => Err(serde::de::Error::invalid_type(
            unexpected_json(&other),
            &"`_behaviors` to be an object (the array form is spelled `behaviors`)",
        )),
    }
}

/// `behaviors` is an object or an array of behavior objects; a scalar used to normalize to no block
/// at all, silently dropping what the author wrote.
fn deserialize_behaviors<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        block @ (None | Some(serde_json::Value::Object(_) | serde_json::Value::Array(_))) => {
            if let Some(inner) = &block {
                refuse_inverted_waits_in_block(inner).map_err(serde::de::Error::custom)?;
            }
            Ok(block)
        }
        Some(other) => Err(serde::de::Error::invalid_type(
            unexpected_json(&other),
            &"`behaviors` to be an object or an array of behavior objects",
        )),
    }
}

/// A compiled behaviors program worth keeping: one with nothing in it says nothing, so there is
/// nothing to run, report or round-trip. `compile_behaviors` already returns `None` for it; this
/// keeps a programmatically built empty array to the same rule.
fn non_empty_behaviors(behaviors: Option<serde_json::Value>) -> Option<serde_json::Value> {
    behaviors.filter(|b| b.as_array().is_some_and(|a| !a.is_empty()))
}

fn unexpected_json(value: &serde_json::Value) -> serde::de::Unexpected<'_> {
    use serde::de::Unexpected;
    match value {
        serde_json::Value::Null => Unexpected::Unit,
        serde_json::Value::Bool(b) => Unexpected::Bool(*b),
        serde_json::Value::Number(n) => n
            .as_u64()
            .map(Unexpected::Unsigned)
            .or_else(|| n.as_i64().map(Unexpected::Signed))
            .or_else(|| n.as_f64().map(Unexpected::Float))
            .unwrap_or(Unexpected::Other("number")),
        serde_json::Value::String(s) => Unexpected::Str(s),
        serde_json::Value::Array(_) => Unexpected::Seq,
        serde_json::Value::Object(_) => Unexpected::Map,
    }
}

impl TryFrom<StubResponseRaw> for StubResponse {
    type Error = String;

    fn try_from(raw: StubResponseRaw) -> Result<Self, Self::Error> {
        // First, as Mountebank's `resolve` checks it before anything else in the response.
        refuse_response_type_count(&raw, admission())?;
        // Judged on the block as written, which the compile below consumes; reported last.
        let door_only = refuse_door_only_behavior_shapes(&raw, admission());
        // `_behaviors` when present, else `behaviors`, compiled into the program that runs
        // (issue #1198). Checked before the response type is chosen, so a block on a `proxy`,
        // `inject` or `fault` response is held to the same rule rift-lint applies to every response.
        let mut behaviors = raw
            .underscore_behaviors
            .or(raw.behaviors)
            .and_then(compile_behaviors);
        // Both are checked before the merge, so a malformed `repeat` in the block is refused even
        // when the top-level one replaces it — the rule rift-lint applies to each.
        refuse_unparseable_behaviors(behaviors.as_ref(), admission())?;
        // A `null` one is absent here; a door has refused it below.
        if let Some(repeat) = raw.repeat.clone().filter(|repeat| !repeat.is_null()) {
            let alone = serde_json::json!({ "repeat": repeat });
            refuse_unparseable_behaviors(Some(&alone), admission())?;
            // Appended, so it is the last `repeat` and wins, as it does in Mountebank.
            let mut program = match behaviors {
                Some(serde_json::Value::Array(program)) => program,
                _ => Vec::new(),
            };
            program.push(alone);
            behaviors = compile_behaviors(serde_json::Value::Array(program));
        }
        // After the behaviors, top-level `repeat` included: `admission_check_stub` checks the
        // compiled behaviors first, and a door must name the same first failure it does.
        refuse_bad_conditional(raw.rift.as_ref(), admission())?;
        // Only when the response really is a proxy (`is` wins the priority below).
        if raw.is.is_none() {
            refuse_unknown_proxy_mode(raw.proxy.as_ref(), admission())?;
            refuse_unreadable_generators(raw.proxy.as_ref(), admission())?;
            refuse_proxy_to_path(raw.proxy.as_ref(), admission())?;
            refuse_bad_proxy_identity(raw.proxy.as_ref(), admission())?;
        }
        // Last, so a response that also fails a check `admission_check_stub` re-runs is refused
        // with that check's message, the one `admission_check` names for it.
        door_only?;
        // Priority: is > proxy > inject > fault > rift-script-only. A door has refused two types or
        // none above, so this order only decides what a replayed config admitted before #1551 decodes
        // to; it must not change.
        Ok(if let Some(is_raw) = raw.is {
            StubResponse::new_is(IsResponse::from(is_raw), behaviors, raw.rift.map(Box::new))
        } else if let Some(proxy) = raw.proxy {
            StubResponse::new_proxy(
                proxy,
                raw.rift.map(Box::new),
                non_empty_behaviors(behaviors),
            )
        } else if let Some(inject) = raw.inject {
            StubResponse::new_inject(
                inject,
                raw.rift.map(Box::new),
                non_empty_behaviors(behaviors),
            )
        } else if let Some(fault) = raw.fault {
            StubResponse::Fault {
                fault,
                ignored_rift: raw.rift.map(Box::new),
                ignored_behaviors: non_empty_behaviors(behaviors),
            }
        } else if let Some(rift) = raw.rift {
            // Rift-only response (script generates the response)
            StubResponse::RiftScript {
                rift: Box::new(rift),
                ignored_behaviors: non_empty_behaviors(behaviors),
            }
        } else if raw.status_code.is_some() || raw.body.is_some() || raw.headers.is_some() {
            // Flat / recorded response form (issue #304): top-level statusCode/headers/body with
            // no `is` wrapper is rendered exactly like `is: { … }`. statusCode defaults to 200.
            StubResponse::new_is(
                IsResponse::decoded(raw.status_code, raw.headers, raw.body, raw.mode),
                behaviors,
                None,
            )
        } else {
            // Default to empty Is response: every field is the author's to omit.
            StubResponse::new_is(
                IsResponse::empty(),
                // An empty block would only cost this response the #703 prepared fast path.
                non_empty_behaviors(behaviors),
                None,
            )
        })
    }
}

impl From<StubResponse> for StubResponseOut {
    fn from(response: StubResponse) -> Self {
        let (out, block) = match response {
            StubResponse::Is {
                is,
                behaviors,
                rift,
                ..
            } => (
                StubResponseOut {
                    is: Some(IsResponseOut::from(is)),
                    rift: rift.map(|rift| *rift),
                    ..StubResponseOut::default()
                },
                behaviors,
            ),
            StubResponse::Proxy {
                proxy,
                ignored_rift,
                behaviors,
                ..
            } => (
                StubResponseOut {
                    proxy: Some(*proxy),
                    rift: ignored_rift.map(|rift| *rift),
                    ..StubResponseOut::default()
                },
                behaviors,
            ),
            StubResponse::Inject {
                inject,
                ignored_rift,
                behaviors,
                ..
            } => (
                StubResponseOut {
                    inject: Some(inject),
                    rift: ignored_rift.map(|rift| *rift),
                    ..StubResponseOut::default()
                },
                behaviors,
            ),
            StubResponse::Fault {
                fault,
                ignored_rift,
                ignored_behaviors,
            } => (
                StubResponseOut {
                    fault: Some(fault),
                    rift: ignored_rift.map(|rift| *rift),
                    ..StubResponseOut::default()
                },
                ignored_behaviors,
            ),
            StubResponse::RiftScript {
                rift,
                ignored_behaviors,
            } => (
                StubResponseOut {
                    rift: Some(*rift),
                    ..StubResponseOut::default()
                },
                ignored_behaviors,
            ),
        };
        let (repeat, behaviors) = block.map_or((None, None), behaviors_out);
        StubResponseOut {
            repeat,
            behaviors,
            ..out
        }
    }
}

/// Write a behaviors block in the grammar Mountebank loads (issue #1191), returning the response's
/// `repeat` and its `behaviors` array.
///
/// - `repeat` is a response field in Mountebank, and a `{"repeat": n}` element is refused. A `0`
///   is not written: Mountebank and a config door refuse it (issue #1582), and Rift serves one it
///   replays from an older engine as `1`, which is what an absent `repeat` means.
/// - Elements come in execution order.
/// - Every step is its own element: a `copy`, `lookup` or `shellTransform` holding several items is
///   written one element per item, the only spelling Mountebank accepts (issue #1199).
/// - A step for a key that is not a behavior configures nothing, and an element holding only such
///   keys is refused by Mountebank and by a config door (#1582), so it is written inside the
///   element before it, where both admit it and [`compile_behaviors`] reads it back as the same
///   step. One with no behavior before it goes into the first behavior after it; one with no
///   behavior at all is not written, as Mountebank's upcast of `_behaviors` drops it.
/// - A `null` or empty-list behavior configures nothing and is not written.
fn behaviors_out(
    block: serde_json::Value,
) -> (Option<serde_json::Value>, Option<Vec<serde_json::Value>>) {
    use serde_json::{Map, Value};
    let Some(Value::Array(program)) = compile_behaviors(block) else {
        return (None, None);
    };
    let mut repeat = None;
    let mut elements: Vec<Map<String, Value>> = Vec::new();
    let mut leading = Map::new();
    for element in program {
        let Value::Object(element) = element else {
            continue;
        };
        if let Some(value) = element.get("repeat") {
            repeat = Some(value.clone()).filter(|r| r.as_u64() != Some(0));
            continue;
        }
        let behavior = element
            .keys()
            .any(|key| crate::behaviors::CANONICAL_ORDER.contains(&key.as_str()));
        match elements.last_mut() {
            Some(previous) if !behavior => previous.extend(element),
            _ if !behavior => leading.extend(element),
            _ => {
                let mut element = element;
                element.append(&mut leading);
                elements.push(element);
            }
        }
    }
    let elements: Vec<Value> = elements.into_iter().map(Value::Object).collect();
    (repeat, (!elements.is_empty()).then_some(elements))
}

/// Keys `ResponseBehaviors` holds as lists: several of them are several steps.
const LIST_BEHAVIORS: [&str; 3] = ["copy", "lookup", "shellTransform"];

/// Compile a behaviors block into the ordered program Mountebank runs (issue #1198): a JSON array
/// of single-key elements in execution order, with the response's `repeat` as a first
/// `{"repeat": n}` element. This is the only shape stored on a `StubResponse`, so the parser, the
/// `--allowInjection` gate, stub analysis and the writer all read the program that runs.
///
/// - The object form (`_behaviors`), and each element of the array form that sets several keys,
///   contribute their keys in [`CANONICAL_ORDER`](crate::behaviors::CANONICAL_ORDER), then any
///   other key alphabetically. Array elements contribute in array order.
/// - A `copy`, `lookup` or `shellTransform` list is one step per item; an empty list is none.
/// - A `null` for a key removes every earlier step of that key (issues #1093, #1098). The last
///   `repeat` wins; a `null` one clears it.
/// - Non-object array elements configure nothing and are skipped.
///
/// Idempotent: compiling a compiled program returns it unchanged. `None` when nothing is left.
pub(crate) fn compile_behaviors(block: serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;
    fn push(
        object: serde_json::Map<String, Value>,
        steps: &mut Vec<(String, Value)>,
        repeat: &mut Option<Value>,
    ) {
        let order = crate::behaviors::CANONICAL_ORDER;
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort_by_key(|k| {
            (
                order.iter().position(|o| o == k).unwrap_or(order.len()),
                k.as_str(),
            )
        });
        let keys: Vec<String> = keys.into_iter().cloned().collect();
        let mut object = object;
        for key in keys {
            let Some(value) = object.remove(&key) else {
                continue;
            };
            if key == "repeat" {
                *repeat = (!value.is_null()).then_some(value);
                continue;
            }
            let known = crate::behaviors::CANONICAL_ORDER.contains(&key.as_str());
            match value {
                Value::Null => steps.retain(|(k, _)| *k != key),
                // A list of items is one step per item. An item that is itself a list or `null` is
                // not one: the whole value stays one step, so the parser refuses it as before
                // rather than this flattening or dropping it.
                Value::Array(items)
                    if LIST_BEHAVIORS.contains(&key.as_str())
                        && items.iter().all(|i| !i.is_array() && !i.is_null()) =>
                {
                    steps.extend(items.into_iter().map(|item| (key.clone(), item)));
                }
                // An unknown key configures nothing; an empty one says nothing either.
                Value::Array(items) if !known && items.is_empty() => {}
                value => steps.push((key, value)),
            }
        }
    }
    let mut steps = Vec::new();
    let mut repeat = None;
    match block {
        Value::Object(object) => push(object, &mut steps, &mut repeat),
        Value::Array(elements) => {
            for element in elements {
                match element {
                    Value::Object(object) => push(object, &mut steps, &mut repeat),
                    // A `null` element is absent, as a `null` key is (issues #1093, #1098).
                    Value::Null => {}
                    // Configures nothing: said, not silently dropped (issues #608, #1101).
                    // rift-lint reports the same element as E048.
                    other => tracing::warn!(
                        element = %other,
                        "`_behaviors`/`behaviors` element is not an object and configures nothing"
                    ),
                }
            }
        }
        _ => return None,
    }
    let program: Vec<Value> = repeat
        .map(|r| serde_json::json!({ "repeat": r }))
        .into_iter()
        .chain(steps.into_iter().map(|(k, v)| serde_json::json!({ k: v })))
        .collect();
    (!program.is_empty()).then_some(Value::Array(program))
}

/// The single key and value of a compiled program element.
fn program_element(element: &serde_json::Value) -> Option<(&String, &serde_json::Value)> {
    element.as_object().and_then(|o| o.iter().next())
}

/// Response mode for body handling (Mountebank compatible)
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum ResponseMode {
    /// Body is UTF-8 text (default)
    #[default]
    Text,
    /// Body is base64-encoded binary data
    Binary,
}

/// A canned response: a stub's `is`, and the imposter's `defaultResponse`.
///
/// What the author left unwritten is recorded in `omitted` (issue #1369), so a response can be
/// completed by the imposter's `defaultResponse` ([`IsResponse::completed_by`]) and printed as it
/// was written. A response built in code carries `Omitted::default()` and is complete.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        rename = "DefaultResponse",
        description = "The response served when no stub matches (`defaultResponse`)."
    )
)]
#[serde(rename_all = "camelCase", from = "DefaultResponseRaw")]
pub struct IsResponse {
    #[serde(default = "default_status_code")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "crate::schema::ResponseStatusCode")
    )]
    pub status_code: u16,
    #[serde(default)]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::MultiValueHeaders")
    )]
    pub headers: Headers,
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    #[serde(rename = "_mode", default)]
    pub mode: ResponseMode,
    /// Which of `status_code`/`headers`/`mode` the author did not write; their fields above then
    /// hold the defaults.
    #[serde(skip)]
    pub(crate) omitted: Omitted,
    /// A `statusCode` string that is not a number (issue #1558), kept as written: `copy` and
    /// `lookup` substitute into it, and it is read at serve time as Mountebank reads it. When set,
    /// `status_code` holds `200` and is not served.
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub(crate) status_text: Option<Box<str>>,
    /// A `proxy` written in `defaultResponse`. Mountebank does not proxy from it and neither does
    /// this engine (`defaultForward` is the feature); it is kept so the printed and persisted
    /// config says what was written, and reported by `ignored_config_keys`. Boxed: it is `None`
    /// on every stub's `is`.
    #[serde(default)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    pub(crate) proxy: Option<Box<serde_json::Value>>,
    /// A stub's `is` only: the `_proxyResponseTime` a proxy recording carries (issue #1519), kept
    /// as written. A `defaultResponse` neither reads nor prints it, as before.
    #[serde(skip)]
    pub(crate) proxy_response_time: Option<serde_json::Value>,
}

/// The printed form of [`IsResponse`]: a field the author omitted is not written.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefaultResponseOut<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    status_code: Option<StatusValue>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_present_headers"
    )]
    headers: Option<&'a Headers>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<&'a serde_json::Value>,
    #[serde(rename = "_mode", skip_serializing_if = "Option::is_none")]
    mode: Option<&'a ResponseMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy: Option<&'a serde_json::Value>,
}

impl Serialize for IsResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        DefaultResponseOut {
            status_code: (!self.omitted.status_code).then(|| self.status_value()),
            // As a default, `{}` and no headers both supply none, so `{}` is not printed (#1361).
            // A stub's `is` keeps a written `{}`: it replaces the default's headers (#1369).
            headers: (!self.omitted.headers && !self.headers.is_empty()).then_some(&self.headers),
            body: self.body.as_ref(),
            mode: (!self.omitted.mode).then_some(&self.mode),
            proxy: self.proxy.as_deref(),
        }
        .serialize(serializer)
    }
}

/// The decode form of [`IsResponse`]: every field optional, so presence survives. It is also what
/// the published JSON Schema describes (`serde(from)` makes `IsResponse` delegate here), so the
/// schema attributes live on this type.
#[derive(Debug, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        rename = "DefaultResponse",
        description = "The response served when no stub matches (`defaultResponse`)."
    )
)]
#[serde(rename_all = "camelCase")]
struct DefaultResponseRaw {
    #[serde(default, deserialize_with = "present_status_value")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "crate::schema::ResponseStatusCode")
    )]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_status_code"))]
    status_code: Option<StatusValue>,
    #[serde(default, deserialize_with = "present_headers")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::MultiValueHeaders")
    )]
    #[cfg_attr(feature = "schema", schemars(default = "schema_default_headers"))]
    headers: Option<Headers>,
    body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    // `skip_serializing_if` is inert for a decode-only type; schemars reads it as "no default".
    #[serde(
        rename = "_mode",
        default,
        deserialize_with = "present_mode",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "ResponseMode"))]
    mode: Option<ResponseMode>,
    /// See [`IsResponse::proxy`]: kept and reported, not part of the published grammar.
    #[cfg_attr(feature = "schema", schemars(skip))]
    proxy: Option<Box<serde_json::Value>>,
}

impl From<DefaultResponseRaw> for IsResponse {
    fn from(raw: DefaultResponseRaw) -> Self {
        IsResponse {
            proxy: raw.proxy,
            ..IsResponse::decoded(raw.status_code, raw.headers, raw.body, raw.mode)
        }
    }
}

impl Default for IsResponse {
    /// The complete `200` with no headers and no body.
    fn default() -> Self {
        IsResponse::new(
            default_status_code(),
            Headers::new(),
            None,
            ResponseMode::Text,
        )
    }
}

impl IsResponse {
    /// A complete response: nothing is omitted, so no `defaultResponse` completes it.
    #[must_use]
    pub fn new(
        status_code: u16,
        headers: Headers,
        body: Option<serde_json::Value>,
        mode: ResponseMode,
    ) -> Self {
        IsResponse {
            status_code,
            headers,
            body,
            mode,
            omitted: Omitted::default(),
            status_text: None,
            proxy: None,
            proxy_response_time: None,
        }
    }

    /// A response that writes nothing, as `{}` does: `200`, no headers, text, and every field
    /// open to a `defaultResponse`.
    pub(crate) fn empty() -> Self {
        IsResponse::decoded(None, None, None, None)
    }

    /// A response as decoded from JSON: a field that was not written takes today's default and is
    /// recorded as omitted.
    pub(crate) fn decoded(
        status: Option<StatusValue>,
        headers: Option<Headers>,
        body: Option<serde_json::Value>,
        mode: Option<ResponseMode>,
    ) -> Self {
        let omitted = Omitted {
            status_code: status.is_none(),
            headers: headers.is_none(),
            mode: mode.is_none(),
            body: body.is_none(),
        };
        let (status_code, status_text) = match status {
            Some(StatusValue::Code(code)) => (code, None),
            Some(StatusValue::Text(text)) => (default_status_code(), Some(text)),
            None => (default_status_code(), None),
        };
        IsResponse {
            omitted,
            status_code,
            status_text,
            headers: headers.unwrap_or_default(),
            body,
            mode: mode.unwrap_or_default(),
            proxy: None,
            proxy_response_time: None,
        }
    }

    /// The `statusCode` as written: the string when it is not a number (issue #1558).
    pub(crate) fn status_value(&self) -> StatusValue {
        match &self.status_text {
            Some(text) => StatusValue::Text(text.clone()),
            None => StatusValue::Code(self.status_code),
        }
    }

    /// Whether a proxy recorded this response: its `_proxyResponseTime` is a number, Mountebank's
    /// `typeof === 'number'` (issue #1519).
    pub(crate) fn is_recorded(&self) -> bool {
        matches!(self.proxy_response_time, Some(serde_json::Value::Number(_)))
    }

    /// Whether `default` has anything to add: a field was omitted (an omitted `_mode` only counts
    /// when the default's differs, since it would change nothing) or there is no body.
    pub(crate) fn needs_default(&self, default: &IsResponse) -> bool {
        self.omitted.status_code
            || self.omitted.headers
            || self.omitted.body
            || (self.omitted.mode && default.mode != self.mode)
    }

    /// Mountebank's `postProcess`, by presence: a field this response omits takes the default's.
    ///
    /// `headers` is taken whole, never merged key by key. `body` is taken when there is none,
    /// `_mode` independently of both. A default that itself omits `statusCode` gives `200`. The
    /// result is complete, so it is not completed again.
    ///
    /// Mountebank decides by falsiness, so it also replaces an explicit `"body": ""`; this goes
    /// by what the response does not specify.
    pub(crate) fn completed_by(&self, default: &IsResponse) -> std::borrow::Cow<'_, IsResponse> {
        if !self.needs_default(default) {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut merged = self.clone();
        if self.omitted.status_code {
            merged.status_code = default.status_code;
            merged.status_text.clone_from(&default.status_text);
        }
        if self.omitted.headers {
            merged.headers.clone_from(&default.headers);
        }
        if self.omitted.body {
            merged.body.clone_from(&default.body);
        }
        if self.omitted.mode {
            merged.mode = default.mode.clone();
        }
        merged.omitted = Omitted::default();
        std::borrow::Cow::Owned(merged)
    }
}

fn is_text_mode(mode: &ResponseMode) -> bool {
    *mode == ResponseMode::Text
}

/// Path rewrite configuration for proxy responses (Mountebank compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PathRewrite {
    /// Pattern to match in the path (string to replace)
    pub from: String,
    /// Replacement string
    pub to: String,
}

// `Debug` is written out below: `key` and `passphrase` are secrets and must not reach a log.
#[derive(Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ProxyResponse {
    pub to: String,
    // The options below are not printed at their default (issue #1361); each decodes back to it.
    // schemars reads `skip_serializing_if` as "no default", so the schema states each one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "crate::schema::proxy_mode", extend("default" = ""))
    )]
    pub mode: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(
            schema_with = "crate::schema::predicate_generators",
            extend("default" = [])
        )
    )]
    pub predicate_generators: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[cfg_attr(feature = "schema", schemars(extend("default" = false)))]
    pub add_wait_behavior: bool,
    // Boxed (issue #1606): the insertion-ordered map is 72 bytes, and `ProxyResponse` is the
    // largest `StubResponse` variant, whose size every stub response pays (#1206, #1209). A plain
    // comment, so the published schema's description is unchanged.
    #[serde(
        default,
        deserialize_with = "boxed_single_value_headers",
        skip_serializing_if = "SingleHeaders::is_empty"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(
            with = "rift_types::schema::SingleValueHeaders",
            extend("default" = {})
        )
    )]
    pub inject_headers: Box<SingleHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_decorate_behavior: Option<String>,
    /// Path rewrite configuration for transforming the request path before proxying
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_rewrite: Option<PathRewrite>,
    /// PEM client certificate (leaf first) presented to an upstream that requests one (issue
    /// #1609, Mountebank-compatible). Paired with `key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    /// PEM private key for `cert`: unencrypted PKCS#8, PKCS#1 or SEC1, or an encrypted PKCS#8
    /// `ENCRYPTED PRIVATE KEY` decrypted with `passphrase` (issue #1609, Mountebank-compatible).
    /// Printed back by `GET /imposters`, as Mountebank prints it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Passphrase for an encrypted `key` (issue #1609, Mountebank-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
}

impl std::fmt::Debug for ProxyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `key` and `passphrase` are operator secrets (issue #1609): shown only as present or not.
        let redacted = |secret: &Option<String>| secret.as_ref().map(|_| "<redacted>");
        f.debug_struct("ProxyResponse")
            .field("to", &self.to)
            .field("mode", &self.mode)
            .field("predicate_generators", &self.predicate_generators)
            .field("add_wait_behavior", &self.add_wait_behavior)
            .field("inject_headers", &self.inject_headers)
            .field("add_decorate_behavior", &self.add_decorate_behavior)
            .field("path_rewrite", &self.path_rewrite)
            .field("cert", &self.cert.as_ref().map(|_| "<pem>"))
            .field("key", &redacted(&self.key))
            .field("passphrase", &redacted(&self.passphrase))
            .finish()
    }
}

impl ProxyResponse {
    /// The client identity this proxy presents (issue #1609): `(cert, key, passphrase)`, or `None`
    /// when it presents none. An empty `cert` or `key` is absent, as it is to Node, which
    /// Mountebank hands them to; a `passphrase` without a key is unused there too.
    #[must_use]
    pub fn client_identity_pem(&self) -> Option<(&str, &str, Option<&str>)> {
        fn present(text: Option<&String>) -> Option<&str> {
            text.map(String::as_str).filter(|text| !text.is_empty())
        }
        Some((
            present(self.cert.as_ref())?,
            present(self.key.as_ref())?,
            present(self.passphrase.as_ref()),
        ))
    }

    /// The recording mode (issue #1314): see [`crate::recording::ProxyMode::parse`]. The string
    /// field is kept as written, so `GET /imposters` echoes it and stored configs are unchanged.
    pub fn mode(&self) -> Result<crate::recording::ProxyMode, String> {
        crate::recording::ProxyMode::parse(&self.mode).ok_or_else(|| {
            format!(
                "unknown proxy mode `{}`; expected proxyOnce, proxyAlways or proxyTransparent \
                 (or omit `mode` for proxyOnce)",
                self.mode
            )
        })
    }
}

/// Refuse a response with more than one of `is`, `proxy`, `inject` and `fault`, or with none, at a
/// config door, in Mountebank's words (issue #1551): `resolve` refuses two types first
/// (`responseResolver.js` `hasMultipleTypes`) and `processResponse` refuses none, and its dry run
/// checks every response of every stub. Types are counted by Mountebank's truthiness: `null` and an
/// empty `inject`/`fault` string are absent. A `_rift`-only response and the flat
/// `statusCode`/`headers`/`body` form (#304) are Rift extensions and still count as a response.
///
/// **Door-only.** [`admission_check_stub`] cannot re-run it: the decoded [`StubResponse`] holds one
/// type, so what this refuses is already gone, and what it serialises never carries two or none.
/// A replayed decode ([`Admission::Skipped`]) keeps the precedence reduction older engines applied.
fn refuse_response_type_count(raw: &StubResponseRaw, admission: Admission) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    let truthy = |text: &Option<String>| text.as_deref().is_some_and(|text| !text.is_empty());
    let types = [
        raw.is.is_some(),
        raw.proxy.is_some(),
        truthy(&raw.inject),
        truthy(&raw.fault),
    ]
    .into_iter()
    .filter(|present| *present)
    .count();
    let flat = raw.status_code.is_some() || raw.body.is_some() || raw.headers.is_some();
    if types > 1 {
        Err("each response object must have only one response type".to_string())
    } else if types == 0 && raw.rift.is_none() && !flat {
        Err("unrecognized response type".to_string())
    } else {
        Ok(())
    }
}

/// Refuse a `proxy` response whose `mode` the engine does not know (issue #1314), at a config door
/// only: a stored config an older engine admitted still decodes under [`Admission::Skipped`].
fn refuse_unknown_proxy_mode(
    proxy: Option<&ProxyResponse>,
    admission: Admission,
) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    proxy.map_or(Ok(()), |proxy| proxy.mode().map(|_| ()))
}

/// Refuse a `proxy` client identity (`cert`/`key`/`passphrase`, issue #1609) the engine could
/// never present, at a config door only. Mountebank admits any of these and then answers every
/// proxied request 500; Rift refuses them at creation instead (a documented difference). Every
/// engine before this one admitted and stored these keys (it dropped them), so this is never a
/// decode refusal: a replayed config with a bad identity still decodes, and its requests 502.
///
/// A `passphrase` on a key that is not encrypted is admitted with a warning: Node ignores it.
fn refuse_bad_proxy_identity(
    proxy: Option<&ProxyResponse>,
    admission: Admission,
) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    let Some(proxy) = proxy else {
        return Ok(());
    };
    let present = |text: &Option<String>| text.as_deref().is_some_and(|text| !text.is_empty());
    match (present(&proxy.cert), present(&proxy.key)) {
        (false, false) => return Ok(()),
        (true, false) => return Err("proxy.cert is given without proxy.key".to_string()),
        (false, true) => return Err("proxy.key is given without proxy.cert".to_string()),
        (true, true) => {}
    }
    let Some((cert, key, passphrase)) = proxy.client_identity_pem() else {
        return Ok(());
    };
    // Charged to the door's `AdmissionBudget`, when it entered one (issue #1609).
    let passphrase_ignored = crate::proxy::client_identity::admit(cert, key, passphrase)?;
    if passphrase_ignored {
        tracing::warn!(
            to = %proxy.to,
            "proxy.passphrase is set but proxy.key is not encrypted; the passphrase is unused"
        );
    }
    Ok(())
}

/// Refuse an `http(s)` `proxy.to` with a path, or a `to` that is not a URL, at a config door only
/// (issue #1550), in Mountebank's words: its dry run parses `to` with WHATWG `new URL` and refuses
/// a pathname other than `/` (`dryRunValidator.js`). `url::Url` is WHATWG too, so `/.` and `/%2e/`
/// normalise to `/` and are admitted, as they are there. A trailing `/`, a query or a fragment is
/// admitted: the forward sends only the request's own path and query ([`upstream_url`]). Every
/// engine before this one admitted and stored such a `to`, so it is never a decode refusal.
fn refuse_proxy_to_path(proxy: Option<&ProxyResponse>, admission: Admission) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    let Some(proxy) = proxy else {
        return Ok(());
    };
    let url = url::Url::parse(&proxy.to)
        .map_err(|_| format!("proxy.to is not a valid URL: {}", proxy.to))?;
    if url.scheme().starts_with("http") && url.path() != "/" {
        return Err(format!("proxy.to must not contain a path '{}'", url.path()));
    }
    Ok(())
}

/// How [`upstream_url`] treats the path of the URL it forwards to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpstreamTarget {
    /// A stub's `proxy.to`: an origin. Only its scheme, host and port are used, as Mountebank's
    /// `getProxyRequest` uses them; its path, query, fragment and userinfo never reach the upstream.
    Origin,
    /// `defaultForward`, a Rift extension documented as a base URL: its path prefixes the request's,
    /// without a doubled `/`; its query, fragment and userinfo are dropped.
    Base,
}

/// The URL a proxy sends a request to (issue #1550): `to`'s scheme and authority, then the
/// (already rewritten) request path and the request's query. It used to be `to` + path + query as a
/// string, so `to` ending in `/` sent `//path`, a `?query` in `to` swallowed the request into the
/// query, and a `#fragment` dropped the path.
///
/// A `to` that does not parse is admitted only from a stored or programmatic config (the doors
/// refuse it); it is an error the caller answers with the proxy-error 502.
pub(crate) fn upstream_url(
    to: &str,
    target: UpstreamTarget,
    path: &str,
    query: Option<&str>,
) -> Result<url::Url, String> {
    let mut url =
        url::Url::parse(to).map_err(|e| format!("proxy.to `{to}` is not a valid URL: {e}"))?;
    if url.cannot_be_a_base() {
        return Err(format!("proxy.to `{to}` is not a URL with a host"));
    }
    let joined = match target {
        UpstreamTarget::Origin => path.to_string(),
        UpstreamTarget::Base => {
            let base = url.path();
            let base = base.strip_suffix('/').unwrap_or(base);
            format!("{base}{path}")
        }
    };
    url.set_path(&joined);
    url.set_query(query);
    url.set_fragment(None);
    // Fails only for a URL without a host (`unix:/socket`), which no upstream client can reach.
    url.set_username("")
        .and_then(|()| url.set_password(None))
        .map_err(|()| format!("proxy.to `{to}` has no host to drop userinfo from"))?;
    Ok(url)
}

/// Refuse a `predicateGenerators` entry the recorder cannot read, at a config door only, like
/// [`refuse_unknown_proxy_mode`]: Mountebank and older engines admitted these, so this is never a
/// decode refusal. Each entry is checked in order: its shape ([`rift_types::generator_shape_error`],
/// issue #1413), then its `predicateOperator`, which must be a field operator (issue #1353) —
/// absent or `null` keeps the default, `deepEquals`. Either used to record fewer predicates than
/// the generator asked for, down to a match-all stub that answered every later request.
fn refuse_unreadable_generators(
    proxy: Option<&ProxyResponse>,
    admission: Admission,
) -> Result<(), String> {
    if admission == Admission::Skipped {
        return Ok(());
    }
    let Some(proxy) = proxy else {
        return Ok(());
    };
    for (i, generator) in proxy.predicate_generators.iter().enumerate() {
        if let Some(refusal) = rift_types::generator_shape_error(i, generator) {
            return Err(refusal);
        }
        if let Some(refusal) = generator_operator_error(i, generator) {
            return Err(refusal);
        }
    }
    Ok(())
}

/// Why entry `i`'s `predicateOperator` is not a field operator (issue #1353); `None` when it is
/// one, or absent or `null`.
pub(crate) fn generator_operator_error(i: usize, generator: &serde_json::Value) -> Option<String> {
    let operator = generator
        .get("predicateOperator")
        .filter(|v| !v.is_null())?;
    if operator
        .as_str()
        .is_some_and(|op| rift_types::GENERATOR_OPERATORS.contains(&op))
    {
        return None;
    }
    let shown = operator
        .as_str()
        .map_or_else(|| operator.to_string(), str::to_string);
    Some(format!(
        "predicateGenerators[{i}].predicateOperator `{shown}` is not a predicate operator; \
         expected one of {}",
        rift_types::GENERATOR_OPERATORS.join(", ")
    ))
}

// ============================================================================
// Imposter Config
// ============================================================================

fn default_protocol() -> String {
    "http".to_string()
}

fn default_enabled() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if contract
fn is_default_enabled(enabled: &bool) -> bool {
    *enabled
}

/// PEM trust anchor(s) as configured (Mountebank `ca`): one PEM string or an array of them.
///
/// The spelling is kept, not normalized (issue #1576): Mountebank echoes `ca` exactly as it was
/// posted, so `["X"]` serializes back as `["X"]` and `"X"` as `"X"` — in the stored form and in
/// every printed view. The SDK parse-fidelity corpus compares reconstructed documents, so turning
/// one spelling into the other is a real difference, not a cosmetic one. Read the anchors through
/// [`PemList::anchors`], which is the same for both spellings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PemList {
    /// Posted as a bare PEM string.
    One(String),
    /// Posted as an array (of any length, one included).
    Many(Vec<String>),
}

impl PemList {
    /// The anchors, whichever spelling they were posted in.
    #[must_use]
    pub fn anchors(&self) -> &[String] {
        match self {
            Self::One(pem) => std::slice::from_ref(pem),
            Self::Many(pems) => pems,
        }
    }
}

impl Serialize for PemList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::One(pem) => serializer.serialize_str(pem),
            Self::Many(pems) => pems.serialize(serializer),
        }
    }
}

/// `ca` accepts a single PEM string or an array of them, matching Mountebank; `null` is unset.
///
/// Hand-written rather than `#[serde(untagged)]` for the reason stated on `TcpFaultSpec` below: an
/// untagged enum reports only "data did not match any variant", which at the `POST /imposters`
/// boundary tells an author nothing about what they got wrong.
pub(crate) mod ca_pem_list {
    use super::PemList;
    use serde::Deserialize;
    use serde::de::{Deserializer, Error as _};

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PemList>, D::Error> {
        let value = Option::<serde_json::Value>::deserialize(deserializer)?;
        let Some(value) = value else {
            return Ok(None);
        };
        match value {
            serde_json::Value::Null => Ok(None),
            serde_json::Value::String(pem) => Ok(Some(PemList::One(pem))),
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    serde_json::Value::String(pem) => Ok(pem),
                    other => Err(D::Error::custom(format!(
                        "`ca` array entries must be PEM strings, got {}",
                        kind_of(&other)
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|pems| Some(PemList::Many(pems))),
            other => Err(D::Error::custom(format!(
                "`ca` must be a PEM string or an array of them, got {}",
                kind_of(&other)
            ))),
        }
    }

    fn kind_of(v: &serde_json::Value) -> &'static str {
        match v {
            serde_json::Value::Null => "null",
            serde_json::Value::Bool(_) => "a boolean",
            serde_json::Value::Number(_) => "a number",
            serde_json::Value::String(_) => "a string",
            serde_json::Value::Array(_) => "an array",
            serde_json::Value::Object(_) => "an object",
        }
    }
}

/// Mountebank's `allowCORS` as configured (issue #1453): any JSON value, read by JavaScript
/// truthiness as Mountebank's `if (options.allowCORS)` reads it. Written back verbatim when
/// enabled — Mountebank 2.9.2+ prints it in every imposter view — and omitted otherwise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllowCors(Option<serde_json::Value>);

impl AllowCors {
    /// Whether CORS is on: the configured value is JavaScript-truthy.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.value().is_some()
    }

    /// The configured value when it enables CORS — what an imposter view prints.
    #[must_use]
    pub fn value(&self) -> Option<&serde_json::Value> {
        self.0
            .as_ref()
            .filter(|v| super::predicates::json::truthy(v))
    }

    fn is_disabled(&self) -> bool {
        !self.enabled()
    }
}

impl From<bool> for AllowCors {
    fn from(enabled: bool) -> Self {
        Self(Some(serde_json::Value::Bool(enabled)))
    }
}

/// Configuration for creating an imposter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(
    feature = "schema",
    derive(schemars::JsonSchema),
    schemars(
        description = "An imposter: a mock server on one port, with its stubs and Rift extensions.",
        transform = crate::schema::alias_properties
    )
)]
#[serde(rename_all = "camelCase")]
pub struct ImposterConfig {
    /// Port for the imposter. If not specified, an available port will be auto-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Host/IP address to bind the imposter to. Defaults to the server's `--host`, or with none to
    /// all interfaces, dual-stack ("::", reporting IPv4 clients as IPv4-mapped addresses), or IPv4
    /// "0.0.0.0" on a host without IPv6, as Mountebank does. An explicit "0.0.0.0" is IPv4-only.
    /// Use "127.0.0.1" or "localhost" for local-only access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_protocol")]
    #[cfg_attr(feature = "schema", schemars(schema_with = "crate::schema::protocol"))]
    pub protocol: String,
    /// Mountebank's creation-time `mode` (issue #1578): `"binary"` refuses `matches` predicates
    /// and `jsonpath`/`xpath` selectors, as Mountebank's dry run does; any other value, or none, is
    /// text. Creation-time only, as in Mountebank: read by the binary-mode check at the create and
    /// apply doors, then cleared, so it is never printed, persisted or replayed, and a later stub
    /// edit is not held to it (Mountebank's stub routes accept the same stub).
    #[serde(default, skip_serializing)]
    pub mode: Option<String>,
    /// Inline PEM certificate for `protocol: "https"` (Mountebank-compatible). Paired with `key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    /// Inline PEM private key for `protocol: "https"` (Mountebank-compatible). Paired with `cert`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Request and **require** a client certificate (Mountebank `mutualAuth`, issue #977).
    ///
    /// Stricter than Mountebank, deliberately: there `mutualAuth` only *requests* a certificate and
    /// never rejects one — and its implementation gates `requestCert` on `rejectUnauthorized` too,
    /// so a bare `mutualAuth: true` is a no-op. For a mock whose job is to virtualize a server that
    /// demands mutual auth, a client that presents nothing must fail the handshake.
    ///
    /// Without `reject_unauthorized` the chain is not validated — any certificate the peer holds
    /// the key for is accepted. `https` only; `true` on an `http` imposter is a creation error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub mutual_auth: bool,
    /// Validate the client certificate against `ca` (Mountebank `rejectUnauthorized`, issue #977).
    ///
    /// Requires `ca`. Mountebank falls back to Node's bundled public roots when `ca` is absent;
    /// validating a *client* certificate against public CAs is meaningless for a mock, so that
    /// combination is a creation error here rather than a silent fallback.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reject_unauthorized: bool,
    /// PEM trust anchor(s) client certificates must chain to (Mountebank `ca`, issue #977).
    /// Accepts a single string or an array of them, as Mountebank does.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "ca_pem_list::deserialize"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "Option<rift_types::schema::PemList>")
    )]
    pub ca: Option<PemList>,
    /// Mountebank's OpenSSL cipher-list string for the https listener, stored verbatim (issue
    /// #1576). Printed upper-cased in the admin views, as Mountebank prints it, but **not
    /// applied**: the TLS listener uses rustls' default suites, so the imposter reports it as an
    /// ignored key. `https` only, as in Mountebank: on any other protocol the imposter drops it
    /// (not stored, not printed) when it is built — never at decode, so stored bytes still read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ciphers: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub record_requests: bool,
    /// Whether the imposter serves data-plane traffic. Toggled at runtime by
    /// POST /imposters/:port/{enable,disable}; persisted so a restart with
    /// --datadir restores the operator's last decision instead of silently
    /// re-enabling a paused imposter. Serialized only when `false`, so configs
    /// for the (universal) enabled case stay byte-identical.
    #[serde(
        default = "default_enabled",
        skip_serializing_if = "is_default_enabled"
    )]
    pub enabled: bool,
    /// Accepted with no effect (`docs/mountebank/imposters.md`). Printed only when `true`: it is a
    /// Rift-only key that round-trips, and Mountebank never prints it (#1613).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub record_matches: bool,
    #[serde(default)]
    pub stubs: Vec<Stub>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_response: Option<IsResponse>,
    /// Fallback upstream for unmatched requests (issue #196): when set and no stub matches,
    /// the request is transparently forwarded to `defaultForward + path` (no recording).
    /// Takes precedence over `defaultResponse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_forward: Option<String>,
    /// Allow CORS headers (Mountebank compatible): any JSON value, on when JavaScript-truthy, and
    /// printed back verbatim when on. Written as Mountebank spells it, `allowCORS`; `allowCors` —
    /// what releases before #1341 wrote into replayable exports and `--datadir` files — still reads.
    #[serde(
        default,
        rename = "allowCORS",
        skip_serializing_if = "AllowCors::is_disabled",
        alias = "allowCors"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "serde_json::Value"))]
    pub allow_cors: AllowCors,
    /// Strict behavior mode (issue #375): when true, a requested response behavior that FAILS
    /// (decorate / shellTransform / binary base64 decode) returns a 500 — still carrying the #323
    /// `x-rift-<behavior>-error` header — instead of serving the fallback body. Default false keeps
    /// the lenient contract (#269): the fallback body is served and only the header signals the
    /// failure. Can also be forced process-wide via the `RIFT_STRICT_BEHAVIORS` env var.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strict_behaviors: bool,
    /// Service name for documentation (optional metadata)
    #[serde(skip_serializing_if = "Option::is_none", alias = "service_name")]
    pub service_name: Option<String>,
    /// Service info for documentation (optional metadata, stored as-is)
    #[serde(skip_serializing_if = "Option::is_none", alias = "service_info")]
    pub service_info: Option<serde_json::Value>,
    /// Rift extensions for advanced features (flow state, scripting, faults)
    #[serde(rename = "_rift", default, skip_serializing_if = "Option::is_none")]
    pub rift: Option<RiftConfig>,
}

// ============================================================================
// Rift Extension Types (_rift namespace)
// ============================================================================

/// Top-level Rift configuration block for imposters
/// Extends Mountebank format with advanced features while maintaining backward compatibility
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftConfig {
    /// Flow state configuration (enables stateful scripting)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_state: Option<RiftFlowStateConfig>,
    /// Metrics configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<RiftMetricsConfig>,
    /// Proxy/upstream configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<RiftProxyConfig>,
    /// Global script engine configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script_engine: Option<RiftScriptEngineConfig>,
    /// Named script registry (issue #356): a response script can reference an entry here via
    /// `{ "ref": "name" }` instead of inlining `code`/`file`. Each entry is itself a `code:` or
    /// `file:` script — not `ref:` (no chains).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scripts: HashMap<String, RiftScriptConfig>,
    /// Response-sequencing configuration: **carried, never executed here.**
    ///
    /// [`ResponseSequencer`] lets an embedder replace the per-stub response cursor, but the trait
    /// is registered once per [`ImposterManager`] — so a backend that should apply to some
    /// imposters and not others, or that needs per-imposter options, has nowhere to read them
    /// from. This block is that surface. Nothing in this crate reads it beyond serializing it
    /// back out, and the built-in cycler behaves identically whether it is present or absent.
    ///
    /// Without a declared field the block would be dropped on parse, since `_rift` has no
    /// catch-all.
    ///
    /// [`ResponseSequencer`]: crate::behaviors::ResponseSequencer
    /// [`ImposterManager`]: crate::imposter::ImposterManager
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(extend("x-rift-carrier" = true)))]
    pub sequencing: Option<RiftSequencingConfig>,
}

/// Per-imposter configuration for a provider-supplied response-cursor backend
/// ([`ResponseSequencer`]).
///
/// Deliberately shaped like [`RiftFlowStateConfig`]: the same problem — an embedder plugging in a
/// seam implementation with nowhere to put its configuration — already solved once for the
/// [`FlowStore`] seam, so the answer is the same one rather than a second convention.
///
/// [`ResponseSequencer`]: crate::behaviors::ResponseSequencer
/// [`FlowStore`]: crate::extensions::flow_state::FlowStore
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftSequencingConfig {
    /// Which cursor backend this imposter wants, interpreted by the registered
    /// [`ResponseSequencer`]. Absent ⇒ the embedder's own default; with no sequencer registered
    /// it means nothing at all.
    ///
    /// [`ResponseSequencer`]: crate::behaviors::ResponseSequencer
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Options belonging to the provider-supplied sequencer, collected rather than dropped —
    /// same reasoning as [`RiftFlowStateConfig::extra`], and this crate reads none of them.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// Hand-written (not derived) because `enabled` and `protocol` default to
// non-zero values; the exhaustive literal makes adding a field without
// deciding its default a compile error rather than a silent zero.
impl Default for ImposterConfig {
    fn default() -> Self {
        Self {
            port: None,
            host: None,
            protocol: default_protocol(),
            mode: None,
            cert: None,
            key: None,
            // Client auth is off unless asked for (issue #977): an imposter that silently began
            // demanding client certificates would break every existing SUT.
            mutual_auth: false,
            reject_unauthorized: false,
            ca: None,
            ciphers: None,
            name: None,
            record_requests: false,
            enabled: default_enabled(),
            record_matches: false,
            stubs: Vec::new(),
            default_response: None,
            default_forward: None,
            allow_cors: AllowCors::default(),
            strict_behaviors: false,
            service_name: None,
            service_info: None,
            rift: None,
        }
    }
}

impl ImposterConfig {
    /// Removes `ciphers` unless this imposter terminates TLS, and returns what was removed.
    ///
    /// `ciphers` is an https key: Mountebank's `httpsServer` reads it and its http server drops it
    /// without a word (issue #1576). Dropped where an imposter is built and where `apply_config`
    /// compares configs — never at decode, so an older engine's stored bytes still decode.
    pub(crate) fn take_non_https_ciphers(&mut self) -> Option<String> {
        if self.protocol.eq_ignore_ascii_case("https") {
            None
        } else {
            self.ciphers.take()
        }
    }

    /// The engine for a script that names none and whose `file` extension does not decide it:
    /// `_rift.scriptEngine.defaultEngine`, else `"rhai"` (issue #1159). The value is not checked
    /// here — an unknown one is refused where a script is actually built with it, so a config that
    /// carries a stale `"lua"` but no engine-less script still loads.
    #[must_use]
    pub fn default_script_engine(&self) -> &str {
        self.rift
            .as_ref()
            .and_then(|rift| rift.script_engine.as_ref())
            .map_or("rhai", |engine| engine.default_engine.as_str())
    }

    /// The port the author pinned, if any. `Some(0)` reads as absent: `0` is the "any free port"
    /// idiom and is auto-assigned at creation (issue #637), so every door that asks whether a port
    /// is explicit — creation, `apply_config`, cross-source claims — agrees on it (issue #1104).
    #[must_use]
    pub fn explicit_port(&self) -> Option<u16> {
        self.port.filter(|port| *port != 0)
    }
}

/// Flow state configuration for Rift extensions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftFlowStateConfig {
    /// Default TTL for state entries in seconds
    #[serde(default = "default_flow_ttl")]
    pub ttl_seconds: i64,
    /// Source for the correlation `flow_id`: `"imposter_port"` (default) or `"header:<Name>"`.
    /// Flattened directly under `flowState` (issue #266). Absent ⇒ `"imposter_port"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id_source: Option<String>,
    /// Options belonging to a provider-supplied store ([`FlowStoreProvider`]).
    ///
    /// An embedder that plugs in its own [`FlowStore`] has, today, nowhere to put
    /// that store's configuration: every field here describes the built-in in-memory store,
    /// and adding one per embedder does not scale (nor should this crate learn
    /// their vocabulary). Unrecognised keys under `flowState` therefore collect
    /// here instead of being dropped, and the provider parses what it owns.
    ///
    /// The built-in in-memory store ignores this entirely — it reads only the typed fields
    /// above, so an unknown key changes nothing for it. This is also what keeps a config written
    /// before `backend` was removed (issue #1463) decodable: a leftover `backend` key, or a
    /// `redis: {…}` block, lands here and selects nothing (rift-cluster replays stored bytes).
    ///
    /// [`FlowStoreProvider`]: crate::extensions::flow_state::FlowStoreProvider
    /// [`FlowStore`]: crate::extensions::flow_state::FlowStore
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_flow_ttl() -> i64 {
    300
}

impl Default for RiftFlowStateConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: default_flow_ttl(),
            flow_id_source: None,
            extra: serde_json::Map::new(),
        }
    }
}

/// Metrics configuration for Rift extensions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftMetricsConfig {
    /// Enable metrics collection
    #[serde(default)]
    pub enabled: bool,
    /// Metrics server port
    #[serde(default = "default_metrics_port")]
    pub port: u16,
}

fn default_metrics_port() -> u16 {
    9090
}

/// Proxy configuration for Rift extensions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftProxyConfig {
    /// Upstream target configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<RiftUpstreamConfig>,
    /// Connection pool settings
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_pool: Option<RiftConnectionPoolConfig>,
}

/// Upstream configuration for Rift proxy
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftUpstreamConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_upstream_protocol")]
    pub protocol: String,
}

fn default_upstream_protocol() -> String {
    "http".to_string()
}

/// Connection pool configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftConnectionPoolConfig {
    #[serde(default = "default_max_idle")]
    pub max_idle_per_host: usize,
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
}

fn default_max_idle() -> usize {
    100
}

fn default_idle_timeout() -> u64 {
    90
}

/// Global script engine configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftScriptEngineConfig {
    /// Engine for a script that names none and has no `.rhai`/`.js`/`.lua` file extension:
    /// "rhai" or "javascript" (issue #1159). See [`ImposterConfig::default_script_engine`].
    #[serde(default = "default_script_engine")]
    pub default_engine: String,
    /// Script execution timeout in milliseconds
    #[serde(default = "default_script_timeout")]
    pub timeout_ms: u64,
}

fn default_script_engine() -> String {
    "rhai".to_string()
}

fn default_script_timeout() -> u64 {
    // Single source of truth with the handler's fallback (issue #308).
    crate::scripting::DEFAULT_SCRIPT_TIMEOUT_MS
}

/// Rift response extensions (added to stub responses)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftResponseExtension {
    /// Fault injection configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<RiftFaultConfig>,
    /// Script-based response generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<RiftScriptConfig>,
    /// Opt-in declarative response templating (issue #359): when `true`, `{{ function args |
    /// filter }}` tokens in the response body and every header value are evaluated (see
    /// `crate::extensions::template_fn`). Off by default so recorded fixtures containing a
    /// literal `{{` are served verbatim.
    #[serde(default)]
    pub templated: bool,
    /// Declarative post-response flow-state writes (issue #969): run in order against the
    /// request's resolved flow id after the response is rendered, just before it is written (see
    /// `crate::extensions::state_ops`). **`is` responses only** — carried here because this block
    /// belongs to an `is` response; on a `proxy`, `inject` or script-only response it is not run
    /// (`stub_analysis` warns). Absent or empty ⇒ nothing runs and the response is served exactly
    /// as before, prepared fast path included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state_ops: Vec<crate::extensions::state_ops::StateOp>,
    /// A dataset-named `lookup` (`_rift.dataset`): **carried, never executed here.**
    ///
    /// A `lookup` behavior needs a filesystem path, which is node-local and so cannot travel in a
    /// config that replicates. `rift-cluster` resolves the dataset on each node and rewrites this
    /// into a real `behaviors.lookup` at apply time; standalone Rift serves a response carrying
    /// one exactly as if it were absent. The field lives here so the declarative form survives a
    /// config round-trip — without it the block is dropped on parse and the binding cannot be
    /// stored at all. See [`crate::behaviors::DatasetBinding`].
    ///
    /// Boxed (issue #1206): this extension is embedded by value in the `Is` variant of
    /// `StubResponse`, which sizes every response, so the binding's 192 bytes were charged to
    /// every response whether or not one was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(extend("x-rift-carrier" = true)))]
    pub dataset: Option<Box<crate::behaviors::DatasetBinding>>,
    /// Declarative conditional GET (issue #1280): an `ETag` and a `Last-Modified` on the response,
    /// and a `304 Not Modified` to a request whose `If-None-Match` / `If-Modified-Since` matches
    /// them. **`is` responses only**, and only for a `GET`/`HEAD` answered with a 2xx (see
    /// `crate::imposter::conditional`). Echoed exactly as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditional: Option<ConditionalGet>,
}

/// `_rift.conditional`: `true`/`false`, or the validators spelled out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum ConditionalGet {
    /// `true` is `{"etag": true, "lastModified": "load"}`; `false` turns the feature off.
    Enabled(bool),
    Validators(ConditionalValidators),
}

/// The spelled-out form of [`ConditionalGet`]. Both fields keep their absence, so the admin API
/// echoes what was declared rather than the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ConditionalValidators {
    /// Whether to send a strong `ETag` over the served bytes. Absent means `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<bool>,
    /// Where `Last-Modified` comes from. Absent means [`LastModified::Load`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<LastModified>,
}

/// `lastModified`: `"load"`, or a fixed HTTP-date served verbatim.
///
/// Any string decodes: a fixed date that does not parse is refused by
/// [`admission_check_stub`], never by the decode, so a stored config always replays (issue #1268).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum LastModified {
    /// The time the stub was loaded: set when it was created or last changed, kept across a
    /// reload that leaves it unchanged.
    Load,
    Fixed(String),
}

impl From<String> for LastModified {
    fn from(value: String) -> Self {
        if value == "load" {
            LastModified::Load
        } else {
            LastModified::Fixed(value)
        }
    }
}

impl From<LastModified> for String {
    fn from(value: LastModified) -> Self {
        match value {
            LastModified::Load => "load".to_owned(),
            LastModified::Fixed(date) => date,
        }
    }
}

impl ConditionalGet {
    /// `(etag, lastModified)` with the defaults applied, or `None` when switched off.
    #[must_use]
    pub fn resolved(&self) -> Option<(bool, &LastModified)> {
        const LOAD: &LastModified = &LastModified::Load;
        match self {
            ConditionalGet::Enabled(false) => None,
            ConditionalGet::Enabled(true) => Some((true, LOAD)),
            ConditionalGet::Validators(v) => Some((
                v.etag.unwrap_or(true),
                v.last_modified.as_ref().unwrap_or(LOAD),
            )),
        }
    }
}

/// Fault injection configuration for responses
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftFaultConfig {
    /// Latency injection
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<RiftLatencyFault>,
    /// Error injection
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RiftErrorFault>,
    /// TCP-level fault
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp: Option<RiftTcpFault>,
}

/// A `_rift.fault.tcp` fault (issue #531). Either the bare kind string — always fires, the
/// canonical Mountebank-compatible form — or the probabilistic object form, a Rift extension that
/// carries a firing probability. Serialized back in exactly the form it was parsed from (string
/// stays string, object stays object) so `GET /imposters` round-trips, which is what the SDK
/// parse-fidelity gate enforces.
///
/// Deserialization is hand-written (rather than `#[serde(untagged)]`) so a malformed object form
/// gets an actionable error at the `POST /imposters` boundary — an untagged enum only ever reports
/// the opaque "data did not match any variant" — mirroring the diagnostics `rift-lint` emits.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum RiftTcpFault {
    /// Bare form: `"CONNECTION_RESET_BY_PEER"` — always fires (probability 1.0).
    Kind(String),
    /// Probabilistic form: fires with the given probability. `probability` is required (no serde
    /// default): the object form exists solely to carry it, so `{ "type": X }` alone would be a
    /// second spelling of the bare form and break parse-fidelity round-trips.
    Probabilistic {
        probability: f64,
        #[serde(rename = "type")]
        kind: String,
    },
}

impl<'de> Deserialize<'de> for RiftTcpFault {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(kind) => Ok(RiftTcpFault::Kind(kind)),
            serde_json::Value::Object(map) => {
                let probability = match map.get("probability") {
                    None => {
                        return Err(D::Error::custom(
                            "_rift.fault.tcp object form requires a numeric 'probability' (use the bare fault-type string for an always-firing fault)",
                        ));
                    }
                    Some(v) => v.as_f64().ok_or_else(|| {
                        D::Error::custom(
                            "_rift.fault.tcp 'probability' must be a number between 0.0 and 1.0",
                        )
                    })?,
                };
                if !(0.0..=1.0).contains(&probability) {
                    return Err(D::Error::custom(
                        "_rift.fault.tcp 'probability' must be between 0.0 and 1.0",
                    ));
                }
                let kind = match map.get("type") {
                    None => {
                        return Err(D::Error::custom(
                            "_rift.fault.tcp object form requires a string 'type'",
                        ));
                    }
                    Some(v) => v
                        .as_str()
                        .ok_or_else(|| D::Error::custom("_rift.fault.tcp 'type' must be a string"))?
                        .to_string(),
                };
                Ok(RiftTcpFault::Probabilistic { probability, kind })
            }
            _ => Err(D::Error::custom(
                "_rift.fault.tcp must be a fault-type string or an object { probability, type }",
            )),
        }
    }
}

impl RiftTcpFault {
    /// The TCP fault kind string, fed to `TcpFaultKind::parse`.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            Self::Kind(kind) => kind,
            Self::Probabilistic { kind, .. } => kind,
        }
    }

    /// Probability the fault fires: `1.0` for the bare form.
    #[must_use]
    pub fn probability(&self) -> f64 {
        match self {
            Self::Kind(_) => 1.0,
            Self::Probabilistic { probability, .. } => *probability,
        }
    }
}

/// Latency fault configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftLatencyFault {
    /// Probability of fault injection (0.0 to 1.0)
    #[serde(default = "default_probability")]
    pub probability: f64,
    /// Minimum latency in milliseconds
    #[serde(default)]
    pub min_ms: u64,
    /// Maximum latency in milliseconds
    #[serde(default)]
    pub max_ms: u64,
    /// Fixed latency (alternative to min/max)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

fn default_probability() -> f64 {
    1.0
}

/// Error fault configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftErrorFault {
    /// Probability of error injection (0.0 to 1.0)
    #[serde(default = "default_probability")]
    pub probability: f64,
    /// HTTP status code for error response
    #[serde(default = "default_error_status")]
    pub status: u16,
    /// Response body for error
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Custom headers for error response
    #[serde(
        default,
        skip_serializing_if = "SingleHeaders::is_empty",
        deserialize_with = "single_value_headers::deserialize"
    )]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "rift_types::schema::SingleValueHeaders")
    )]
    pub headers: SingleHeaders,
}

fn default_error_status() -> u16 {
    503
}

/// Script configuration for response generation.
///
/// Exactly one of `code`, `file`, or `ref` must be set (issue #356). This is enforced by the
/// config-time script-resolution pass (`imposter::script_resolve`) rather than at deserialize
/// time, so a validation error can name the offending stub/registry entry instead of a generic
/// serde failure — and so existing configs using only `code` keep deserializing unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RiftScriptConfig {
    /// Script engine: "rhai" or "javascript". When absent, the resolver infers it from
    /// `file`'s extension (`.rhai`/`.js`; a `.lua` file still resolves to the now-removed "lua"
    /// engine so it is rejected with a clear error rather than silently mis-run), falling back to
    /// "rhai" — the legacy default — when neither `engine` nor an inferable extension is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Inline script code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Path to a script file. Resolved relative to the config file's directory for
    /// `--configfile`/`--datadir` loads, or to `--scripts-dir` for admin-API-created imposters
    /// (a path that escapes `--scripts-dir` is rejected rather than read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Name of a `_rift.scripts` registry entry (see [`RiftConfig::scripts`]) to resolve this
    /// script from. The referenced entry must itself use `code` or `file` — a `ref` chain is a
    /// validation error.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_name: Option<String>,
}

impl RiftScriptConfig {
    /// How many of `code`/`file`/`ref` are set. Exactly one is valid; used by both the resolver
    /// and rift-lint to report "missing source" vs. "multiple sources" distinctly.
    pub fn source_count(&self) -> usize {
        [
            self.code.is_some(),
            self.file.is_some(),
            self.ref_name.is_some(),
        ]
        .into_iter()
        .filter(|set| *set)
        .count()
    }

    /// True when exactly one of `code`/`file`/`ref` is set.
    pub fn has_valid_source(&self) -> bool {
        self.source_count() == 1
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// Error types for imposter management
#[derive(Debug, thiserror::Error)]
pub enum ImposterError {
    /// A config the engine refuses at creation for a reason that is neither its protocol nor its
    /// TLS settings, worded as Mountebank words it (issue #1578: binary-mode selectors).
    #[error("{0}")]
    InvalidConfig(String),
    #[error("Port {0} is already in use")]
    PortInUse(u16),
    #[error("Imposter not found on port {0}")]
    NotFound(u16),
    #[error("Failed to bind port {0}: {1:#}")]
    BindError(u16, anyhow::Error),
    #[error("Invalid protocol: {0}")]
    InvalidProtocol(String),
    #[error("Stub index {0} out of bounds")]
    StubIndexOutOfBounds(usize),
    #[error("No stub with id '{0}'")]
    StubNotFound(String),
    #[error("A stub with id '{0}' already exists")]
    StubIdConflict(String),
    #[error("Failed to persist imposter: {0:#}")]
    PersistError(anyhow::Error),
    #[error("TLS configuration error: {0}")]
    Tls(String),
    #[error("flow store configuration error: {0}")]
    FlowStoreConfig(String),
    #[error("backend error: {0:#}")]
    Backend(anyhow::Error),
    #[error("an explicit port is required to reconcile a single imposter")]
    ExplicitPortRequired,
}

/// JSON Schema for the door types (issue #1342). The wire shape of `Stub`, `StubResponse` and
/// `DelayRange` is their raw type's — that is what `POST /imposters` and a config file accept — so
/// each forwards to it under its own name. `LastModified` and `RiftTcpFault` decode by hand, so
/// their schemas are written by hand too.
#[cfg(feature = "schema")]
mod schema_impls {
    use super::{
        DelayRange, DelayRangeRaw, LastModified, RiftTcpFault, Stub, StubRaw, StubResponse,
        StubResponseRaw,
    };
    use crate::imposter::fault_io::TcpFaultKind;
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use std::borrow::Cow;

    impl JsonSchema for Stub {
        fn schema_name() -> Cow<'static, str> {
            "Stub".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            StubRaw::json_schema(generator)
        }
    }

    impl JsonSchema for StubResponse {
        fn schema_name() -> Cow<'static, str> {
            "StubResponse".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            StubResponseRaw::json_schema(generator)
        }
    }

    impl JsonSchema for DelayRange {
        fn schema_name() -> Cow<'static, str> {
            "DelayRange".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            DelayRangeRaw::json_schema(generator)
        }
    }

    impl JsonSchema for LastModified {
        fn schema_name() -> Cow<'static, str> {
            "LastModified".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "`\"load\"` for the time the stub was loaded, or a fixed HTTP-date \
                                served verbatim.",
                "type": "string"
            })
        }
    }

    impl JsonSchema for RiftTcpFault {
        fn schema_name() -> Cow<'static, str> {
            "RiftTcpFault".into()
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            let kind = serde_json::json!({ "type": "string", "enum": TcpFaultKind::SPELLINGS });
            json_schema!({
                "description": "A transport fault: the bare kind string always fires; the object \
                                form fires with the given probability.",
                "anyOf": [
                    kind,
                    {
                        "type": "object",
                        "properties": {
                            "probability": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                            "type": kind
                        },
                        "required": ["probability", "type"],
                        "additionalProperties": false
                    }
                ]
            })
        }
    }
}

/// Context marker for a bind host that does not resolve. tokio's lookup error has no stable
/// `io::ErrorKind`, so the resolver tags it at the source for [`ImposterError::is_caller_address_error`].
#[derive(Debug)]
pub(crate) struct UnresolvableHost;

impl std::fmt::Display for UnresolvableHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("host does not resolve to a bindable address")
    }
}

impl ImposterError {
    /// A bind failure caused by the address the caller asked for (taken, not local, not
    /// permitted, unresolvable), as opposed to an engine-side fault such as EMFILE. Mountebank
    /// answers these 400.
    #[must_use]
    pub fn is_caller_address_error(&self) -> bool {
        let Self::BindError(_, source) = self else {
            return false;
        };
        source.downcast_ref::<UnresolvableHost>().is_some()
            || source.downcast_ref::<std::io::Error>().is_some_and(|io| {
                matches!(
                    io.kind(),
                    std::io::ErrorKind::AddrInUse
                        | std::io::ErrorKind::AddrNotAvailable
                        | std::io::ErrorKind::PermissionDenied
                )
            })
    }
}

#[cfg(test)]
mod imposter_error_chain_tests {
    use super::ImposterError;

    // Issue #688: BindError/PersistError carry an anyhow::Error now, not a flattened String, so a
    // chain-bearing source reaches the sink intact. Displayed with `{:#}`, the whole chain renders.
    #[test]
    fn bind_error_display_renders_the_whole_chain() {
        let source = anyhow::anyhow!("address already in use (os error 48)")
            .context("SO_REUSEADDR bind of 0.0.0.0:8080");
        let rendered = format!("{}", ImposterError::BindError(8080, source));
        assert!(
            rendered.starts_with("Failed to bind port 8080: "),
            "{rendered}"
        );
        assert!(
            rendered.contains("SO_REUSEADDR bind of 0.0.0.0:8080"),
            "{rendered}"
        );
        assert!(
            rendered.contains("address already in use"),
            "the root cause must survive to the sink: {rendered}"
        );
    }

    #[test]
    fn persist_error_display_renders_the_whole_chain() {
        let source = anyhow::Error::new(std::io::Error::other("no space left on device"))
            .context("write imposter 8080 to \"/data/8080.json\"");
        let rendered = format!("{}", ImposterError::PersistError(source));
        assert!(
            rendered.starts_with("Failed to persist imposter: "),
            "{rendered}"
        );
        assert!(rendered.contains("write imposter 8080"), "{rendered}");
        assert!(
            rendered.contains("no space left on device"),
            "the root cause must survive to the sink: {rendered}"
        );
    }

    // AC2: for a single-level (leaf) source — every construction that exists today — `{:#}` renders
    // exactly what the old `String` field did, so admin `failed[].error` and the hot-reload body are
    // byte-identical. These pin that the type change did not perturb the current messages.
    #[test]
    fn bind_error_leaf_display_is_byte_identical_to_the_old_string() {
        let err = ImposterError::BindError(8080, anyhow::anyhow!("no socket address"));
        assert_eq!(
            format!("{err}"),
            "Failed to bind port 8080: no socket address"
        );
    }

    #[test]
    fn persist_error_leaf_display_is_byte_identical_to_the_old_string() {
        // Mirror the exact shape manager.rs builds — `Error::new(io).context("...")`, a 2-level
        // chain — so this pins the real construction, not a degenerate single-level stand-in. Under
        // `{0:#}` it must render what the old `PersistError(format!("...: {e}"))` String produced.
        let io = std::io::Error::other("no space left on device");
        let err = ImposterError::PersistError(
            anyhow::Error::new(io).context("Failed to serialize imposter 8080"),
        );
        assert_eq!(
            format!("{err}"),
            "Failed to persist imposter: Failed to serialize imposter 8080: no space left on device"
        );
    }
}

#[cfg(test)]
mod tests {

    // ===== Issue #1551: a response carries exactly one type, as in Mountebank =====
    mod response_type_count {
        use super::super::{ImposterConfig, StubResponse, deserialize_replayed};

        fn imposter_with_responses(responses: &str) -> String {
            format!(r#"{{"port":4700,"protocol":"http","stubs":[{{"responses":{responses}}}]}}"#)
        }

        const MULTIPLE_TYPES: &str = "each response object must have only one response type";
        const NO_TYPE: &str = "unrecognized response type";

        /// Every row Mountebank 2.9.5 answers `400` for, with its message (probed in the #1551 triage).
        #[test]
        fn a_response_with_two_types_or_none_is_refused_with_mountebanks_message() {
            let proxy = r#"{"to":"http://localhost:1"}"#;
            let inject = r#""function () { return {}; }""#;
            let fault = r#""CONNECTION_RESET_BY_PEER""#;
            let refused = [
                (
                    format!(r#"[{{"is":{{}},"proxy":{proxy}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"is":{{}},"inject":{inject}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"is":{{}},"fault":{fault}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"proxy":{proxy},"inject":{inject}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"proxy":{proxy},"fault":{fault}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"inject":{inject},"fault":{fault}}}]"#),
                    MULTIPLE_TYPES,
                ),
                (
                    format!(r#"[{{"is":{{}},"proxy":{proxy},"inject":{inject},"fault":{fault}}}]"#),
                    MULTIPLE_TYPES,
                ),
                ("[{}]".to_string(), NO_TYPE),
                (r#"[{"is":null}]"#.to_string(), NO_TYPE),
                (r#"[{"iss":{"statusCode":500}}]"#.to_string(), NO_TYPE),
                (r#"[{"behaviors":[{"wait":10}]}]"#.to_string(), NO_TYPE),
                (r#"[{"_behaviors":{"wait":10}}]"#.to_string(), NO_TYPE),
                (r#"[{"repeat":2}]"#.to_string(), NO_TYPE),
                // #1556's triage: an empty `fault` is no type, as Mountebank's truthiness reads it.
                (r#"[{"fault":""}]"#.to_string(), NO_TYPE),
                (r#"[{"inject":""}]"#.to_string(), NO_TYPE),
                (
                    r#"[{"proxy":null,"inject":"","fault":""}]"#.to_string(),
                    NO_TYPE,
                ),
            ];
            for (responses, message) in refused {
                let error =
                    serde_json::from_str::<ImposterConfig>(&imposter_with_responses(&responses))
                        .expect_err(&responses)
                        .to_string();
                assert!(
                    error.starts_with(message),
                    "{responses}: expected `{message}`, got `{error}`"
                );
            }
        }

        /// The rows Mountebank admits, by the truthiness it counts types with, and Rift's two
        /// extensions: the flat form (#304) and a `_rift`-only response.
        #[test]
        fn a_response_with_one_type_by_mountebanks_truthiness_is_admitted() {
            let admitted = [
                (r#"[{"is":{}}]"#, "is"),
                (r#"[{"is":{},"proxy":null}]"#, "is"),
                (r#"[{"is":{},"inject":""}]"#, "is"),
                (r#"[{"is":{},"fault":""}]"#, "is"),
                (
                    r#"[{"proxy":{"to":"http://localhost:1"},"inject":""}]"#,
                    "proxy",
                ),
                (
                    r#"[{"inject":"function () { return {}; }","fault":""}]"#,
                    "inject",
                ),
                (
                    r#"[{"fault":"CONNECTION_RESET_BY_PEER","proxy":null}]"#,
                    "fault",
                ),
                (r#"[{"is":{},"behaviors":[{"wait":10}],"repeat":2}]"#, "is"),
                (r#"[{"statusCode":201,"body":"flat"}]"#, "is"),
                (r#"[{"headers":{"X-A":"1"}}]"#, "is"),
                (
                    r#"[{"_rift":{"script":{"engine":"rhai","code":"fn respond(ctx) { http(200, #{}) }"}}}]"#,
                    "_rift",
                ),
            ];
            for (responses, kind) in admitted {
                let config: ImposterConfig =
                    serde_json::from_str(&imposter_with_responses(responses))
                        .unwrap_or_else(|e| panic!("{responses}: {e}"));
                let response = &config.stubs[0].responses[0];
                let decoded = match response {
                    StubResponse::Is { .. } => "is",
                    StubResponse::Proxy { .. } => "proxy",
                    StubResponse::Inject { .. } => "inject",
                    StubResponse::Fault { .. } => "fault",
                    StubResponse::RiftScript { .. } => "_rift",
                };
                assert_eq!(decoded, kind, "{responses}");
            }
        }

        /// Mountebank's dry run checks every response, not just the first: a valid first response does
        /// not cover a bad second one, and the error points into the second.
        #[test]
        fn a_bad_second_response_is_refused_and_located() {
            let json = imposter_with_responses(r#"[{"is":{"body":"a"}},{}]"#);
            let error = serde_json::from_str::<ImposterConfig>(&json).expect_err("refused");
            assert!(error.to_string().starts_with(NO_TYPE), "{error}");
            let second = json.find("},{}").expect("second response") + 2;
            assert!(
                error.column() > second,
                "the error must point past the first response (column {} > {second}): {error}",
                error.column()
            );
        }

        /// Door-only (issue #1551): a replayed decode keeps the old reduction, so stored bytes decode to
        /// what they always did.
        #[test]
        fn a_replayed_response_keeps_the_precedence_and_the_empty_default() {
            let replay = |json: &str| -> ImposterConfig {
                let mut deserializer = serde_json::Deserializer::from_str(json);
                deserialize_replayed(&mut deserializer).expect("replayed decode admits")
            };
            let two = replay(&imposter_with_responses(
                r#"[{"is":{"statusCode":201},"proxy":{"to":"http://localhost:1"}}]"#,
            ));
            assert!(matches!(
                &two.stubs[0].responses[0],
                StubResponse::Is { is, .. } if is.status_code == 201
            ));
            let none = replay(&imposter_with_responses("[{}]"));
            assert_eq!(
                serde_json::to_value(&none.stubs[0].responses[0]).expect("print"),
                serde_json::json!({"is": {}})
            );
        }
    }

    // ===== Issue #1362: a stub's `name` is kept and printed first =====

    #[test]
    fn stub_name_round_trips() {
        let stub: super::Stub =
            serde_json::from_str(r#"{"name":"s1","responses":[{"is":{}}]}"#).expect("named stub");
        assert_eq!(
            serde_json::to_value(&stub).expect("print")["name"],
            serde_json::json!("s1")
        );
        let printed = serde_json::to_string(&stub).expect("print");
        assert!(
            printed.starts_with(r#"{"name":"s1","#),
            "name prints first: {printed}"
        );
    }

    // ===== Issue #1377: `defaultResponse.statusCode` reads like every other `statusCode` =====

    #[test]
    fn default_response_status_code_accepts_numeric_string() {
        let config: super::ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "defaultResponse": {"statusCode": "404"}
        }))
        .expect("a numeric-string defaultResponse.statusCode is accepted");
        assert_eq!(
            config
                .default_response
                .expect("defaultResponse")
                .status_code,
            404
        );
    }

    #[test]
    fn default_response_status_code_refuses_non_numeric() {
        // Issue #1558: a string is any string, as Mountebank stores it; a boolean and `null` are
        // still refused.
        for bad in [serde_json::json!(true), serde_json::Value::Null] {
            let parsed = serde_json::from_value::<super::ImposterConfig>(serde_json::json!({
                "protocol": "http",
                "defaultResponse": {"statusCode": bad.clone()}
            }));
            assert!(parsed.is_err(), "statusCode {bad} must be refused");
        }
    }

    #[test]
    fn default_response_status_code_still_prints_as_a_number() {
        let config: super::ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "defaultResponse": {"statusCode": "404"}
        }))
        .expect("accepted");
        let printed = serde_json::to_value(config.default_response.expect("defaultResponse"))
            .expect("serializes");
        assert_eq!(printed["statusCode"], serde_json::json!(404));
    }

    // ===== Issue #1558: any string `statusCode` is kept as written =====

    fn is_of(stub: serde_json::Value) -> super::IsResponse {
        let stub: super::Stub = serde_json::from_value(stub).expect("stub decodes");
        match stub.responses.into_iter().next().expect("one response") {
            super::StubResponse::Is { is, .. } => is,
            other => panic!("expected an `is` response, got {other:?}"),
        }
    }

    #[test]
    fn a_status_token_decodes_prints_back_as_written_and_re_decodes() {
        let stub = serde_json::json!({"responses": [{"is": {"statusCode": "${code}"}}]});
        let is = is_of(stub);
        let printed = serde_json::to_value(super::IsResponseOut::from(is)).expect("prints");
        assert_eq!(printed["statusCode"], serde_json::json!("${code}"));
        let again = is_of(serde_json::json!({"responses": [{"is": printed.clone()}]}));
        assert_eq!(
            serde_json::to_value(super::IsResponseOut::from(again)).expect("prints"),
            printed
        );
    }

    #[test]
    fn a_numeric_string_status_still_prints_as_a_number() {
        let is = is_of(serde_json::json!({"responses": [{"is": {"statusCode": "404"}}]}));
        assert_eq!(is.status_code, 404);
        let printed = serde_json::to_value(super::IsResponseOut::from(is)).expect("prints");
        assert_eq!(printed["statusCode"], serde_json::json!(404));
    }

    #[test]
    fn a_default_response_status_token_prints_back_as_written() {
        let config: super::ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "defaultResponse": {"statusCode": "${x}"}
        }))
        .expect("a token defaultResponse.statusCode is accepted, as Mountebank does");
        let printed = serde_json::to_value(config.default_response.expect("defaultResponse"))
            .expect("serializes");
        assert_eq!(printed["statusCode"], serde_json::json!("${x}"));
    }

    // ===== Issue #1369: `defaultResponse` completes what a response omits =====

    fn decoded(value: serde_json::Value) -> super::IsResponse {
        serde_json::from_value(value).expect("decodes")
    }

    fn default_418() -> super::IsResponse {
        decoded(serde_json::json!({
            "statusCode": 418, "headers": {"X-Default": "yes"}, "body": "dflt", "_mode": "binary"
        }))
    }

    #[test]
    fn completed_by_takes_every_omitted_field() {
        let merged = decoded(serde_json::json!({}))
            .completed_by(&default_418())
            .into_owned();
        assert_eq!(merged.status_code, 418);
        assert_eq!(merged.headers["X-Default"], vec!["yes".to_string()]);
        assert_eq!(merged.body, Some(serde_json::json!("dflt")));
        assert_eq!(merged.mode, super::ResponseMode::Binary);
    }

    #[test]
    fn completed_by_keeps_what_was_written() {
        let own = decoded(serde_json::json!({
            "statusCode": 201, "headers": {"A": "b"}, "body": "mine", "_mode": "text"
        }));
        assert!(matches!(
            own.completed_by(&default_418()),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn completed_by_takes_headers_whole() {
        let merged = decoded(serde_json::json!({"headers": {"A": "b"}}))
            .completed_by(&default_418())
            .into_owned();
        assert_eq!(merged.headers.len(), 1, "never merged key by key");
        assert!(merged.headers.contains_key("A"));
        assert_eq!(merged.status_code, 418);
    }

    #[test]
    fn completed_by_treats_written_empty_values_as_written() {
        let merged = decoded(serde_json::json!({"headers": {}, "body": "", "statusCode": 200}))
            .completed_by(&default_418())
            .into_owned();
        assert!(merged.headers.is_empty(), "an explicit `{{}}` is a value");
        assert_eq!(merged.body, Some(serde_json::json!("")));
        assert_eq!(merged.status_code, 200);
    }

    #[test]
    fn completed_by_falls_back_on_mode_independently() {
        let merged = decoded(serde_json::json!({"statusCode": 201, "headers": {}, "body": "x"}))
            .completed_by(&default_418())
            .into_owned();
        assert_eq!(merged.status_code, 201);
        assert_eq!(merged.mode, super::ResponseMode::Binary);
        let written = decoded(serde_json::json!({"body": "x", "_mode": "text"}))
            .completed_by(&default_418())
            .into_owned();
        assert_eq!(written.mode, super::super::ResponseMode::Text);
        assert_eq!(written.status_code, 418);
    }

    #[test]
    fn completed_by_a_default_that_omits_status_code_gives_200() {
        let default = decoded(serde_json::json!({"body": "d"}));
        let merged = decoded(serde_json::json!({}))
            .completed_by(&default)
            .into_owned();
        assert_eq!(merged.status_code, 200);
        assert_eq!(merged.body, Some(serde_json::json!("d")));
    }

    #[test]
    fn a_response_built_in_code_is_complete() {
        let recorded = super::IsResponse::new(
            200,
            crate::imposter::Headers::new(),
            None,
            super::ResponseMode::Text,
        );
        let merged = recorded.completed_by(&default_418()).into_owned();
        assert_eq!(
            merged.status_code, 200,
            "no field is omitted, so the status stays"
        );
        assert!(merged.headers.is_empty(), "and so do the headers");
        assert_eq!(merged.body, None, "nor does a body that is `None`");
        assert!(matches!(
            recorded.completed_by(&default_418()),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn needs_default_ignores_an_omitted_mode_the_default_shares() {
        let written = decoded(serde_json::json!({"statusCode": 200, "headers": {}, "body": "b"}));
        let text_default = decoded(serde_json::json!({"statusCode": 418}));
        assert!(!written.needs_default(&text_default), "nothing to add");
        assert!(
            written.needs_default(&default_418()),
            "the default is binary"
        );
    }

    #[test]
    fn a_written_mode_prints_even_when_text() {
        let stub: super::StubResponse =
            serde_json::from_value(serde_json::json!({"is": {"_mode": "text"}})).expect("decodes");
        assert_eq!(
            serde_json::to_value(&stub).expect("prints"),
            serde_json::json!({"is": {"_mode": "text"}})
        );
        let recorded = super::StubResponse::new_is(
            super::IsResponse::new(
                200,
                crate::imposter::Headers::new(),
                None,
                super::ResponseMode::Text,
            ),
            None,
            None,
        );
        assert_eq!(
            serde_json::to_value(&recorded).expect("prints")["is"]["_mode"],
            serde_json::json!("text"),
            "a recording prints `_mode`, as Mountebank's recorder does"
        );
    }

    // Issue #1519: `_proxyResponseTime` is the per-response mark Mountebank's clear reads.
    #[test]
    fn a_proxy_response_time_is_kept_and_printed() {
        let written = serde_json::json!({"is": {"body": "u", "_proxyResponseTime": 5}});
        let stub: super::StubResponse = serde_json::from_value(written.clone()).expect("decodes");
        assert_eq!(serde_json::to_value(&stub).expect("prints"), written);
    }

    #[test]
    fn a_non_numeric_proxy_response_time_loads_and_echoes_but_marks_nothing() {
        for value in [serde_json::json!("x"), serde_json::json!(null)] {
            let written = serde_json::json!({"is": {"body": "u", "_proxyResponseTime": value}});
            let stub: super::StubResponse =
                serde_json::from_value(written.clone()).expect("a lenient decode");
            assert_eq!(serde_json::to_value(&stub).expect("prints"), written);
            assert!(
                !stub.is_recorded(),
                "{value}: only a number marks a recording"
            );
        }
        let marked: super::StubResponse = serde_json::from_value(
            serde_json::json!({"is": {"body": "u", "_proxyResponseTime": 0}}),
        )
        .expect("decodes");
        assert!(marked.is_recorded());
    }

    #[test]
    fn a_default_response_ignores_a_proxy_response_time() {
        let config: super::ImposterConfig = serde_json::from_value(serde_json::json!({
            "port": 0, "protocol": "http",
            "defaultResponse": {"body": "d", "_proxyResponseTime": 5}
        }))
        .expect("decodes");
        let printed = serde_json::to_value(&config).expect("prints");
        assert_eq!(
            printed["defaultResponse"],
            serde_json::json!({"body": "d"}),
            "{printed}"
        );
    }

    #[test]
    fn omitted_fields_are_not_printed() {
        let printed =
            serde_json::to_value(decoded(serde_json::json!({"body": "b"}))).expect("prints");
        assert_eq!(printed, serde_json::json!({"body": "b"}));
        let stub: super::StubResponse =
            serde_json::from_value(serde_json::json!({"is": {}})).expect("decodes");
        let printed = serde_json::to_value(&stub).expect("prints");
        assert_eq!(printed, serde_json::json!({"is": {}}));
        let stub: super::StubResponse =
            serde_json::from_value(serde_json::json!({"is": {"statusCode": "200", "headers": {}}}))
                .expect("decodes");
        let printed = serde_json::to_value(&stub).expect("prints");
        // A written `statusCode` prints as a number (issue #1361); a written `headers: {}` stays.
        assert_eq!(
            printed,
            serde_json::json!({"is": {"statusCode": 200, "headers": {}}})
        );
    }

    #[test]
    fn bytes_written_by_earlier_releases_decode_and_read_as_written() {
        let stub: super::StubResponse =
            serde_json::from_value(serde_json::json!({"is": {"statusCode": "200", "headers": {}}}))
                .expect("decodes");
        let super::StubResponse::Is { is, .. } = stub else {
            panic!("an `is` response");
        };
        assert!(
            !is.omitted.status_code && !is.omitted.headers,
            "written, so as written"
        );
    }

    #[test]
    fn flat_form_records_presence_too() {
        let stub: super::StubResponse =
            serde_json::from_value(serde_json::json!({"statusCode": 204})).expect("decodes");
        let super::StubResponse::Is { is, .. } = stub else {
            panic!("the flat form is an `is` response");
        };
        assert!(!is.omitted.status_code);
        assert!(is.omitted.headers);
        assert!(is.omitted.mode);
    }

    #[test]
    fn default_response_proxy_is_kept_and_printed() {
        let default = decoded(serde_json::json!({"proxy": {"to": "http://127.0.0.1:1"}}));
        assert_eq!(
            serde_json::to_value(&default).expect("prints"),
            serde_json::json!({"proxy": {"to": "http://127.0.0.1:1"}})
        );
    }

    #[test]
    fn a_null_in_a_response_is_still_refused_where_it_was() {
        for bad in [
            serde_json::json!({"statusCode": null}),
            serde_json::json!({"_mode": null}),
        ] {
            assert!(
                serde_json::from_value::<super::IsResponse>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    // ===== Issue #1578: binary-mode selectors =====

    fn predicates_of(predicates: serde_json::Value) -> Vec<Predicate> {
        serde_json::from_value(predicates).expect("predicates decode")
    }

    #[test]
    fn binary_mode_refuses_matches_and_selectors_wherever_they_sit() {
        let matches = "the matches predicate is not allowed in binary mode";
        let jsonpath = "the jsonpath predicate parameter is not allowed in binary mode";
        let xpath = "the xpath predicate parameter is not allowed in binary mode";
        for (predicates, expected) in [
            (serde_json::json!([{ "matches": { "body": "x" } }]), matches),
            (
                serde_json::json!([{ "equals": { "body": "x" }, "jsonpath": { "selector": "$.a" } }]),
                jsonpath,
            ),
            (
                serde_json::json!([{ "equals": { "body": "x" }, "xpath": { "selector": "//a" } }]),
                xpath,
            ),
            (
                serde_json::json!([{ "exists": { "body": true }, "jsonpath": { "selector": "$.a" } }]),
                jsonpath,
            ),
            (
                serde_json::json!([{ "not": { "matches": { "body": "x" } } }]),
                matches,
            ),
            (
                serde_json::json!([{ "and": [{ "equals": { "path": "/" } },
                                             { "not": { "matches": { "body": "x" } } }] }]),
                matches,
            ),
            (
                serde_json::json!([{ "or": [{ "equals": { "path": "/" } },
                                            { "exists": { "body": true },
                                              "xpath": { "selector": "//a" } }] }]),
                xpath,
            ),
        ] {
            assert_eq!(
                refuse_binary_mode_selectors(&predicates_of(predicates.clone())),
                Err(expected.to_string()),
                "{predicates}"
            );
        }
    }

    #[test]
    fn binary_mode_admits_plain_predicates() {
        let predicates = predicates_of(serde_json::json!([
            { "equals": { "body": "AQID" } },
            { "deepEquals": { "path": "/" } },
            { "exists": { "body": true } },
            { "not": { "contains": { "body": "x" } } }
        ]));
        assert_eq!(refuse_binary_mode_selectors(&predicates), Ok(()));
    }

    /// Mountebank selects before `matches` throws, so a `matches` with a selector names the selector.
    #[test]
    fn binary_mode_names_the_selector_before_matches() {
        let predicates = predicates_of(serde_json::json!([
            { "matches": { "body": "x" }, "jsonpath": { "selector": "$.a" } }
        ]));
        assert_eq!(
            refuse_binary_mode_selectors(&predicates),
            Err("the jsonpath predicate parameter is not allowed in binary mode".to_string())
        );
    }

    /// Only an exact `"binary"` is binary, as in Mountebank's `request.mode === 'binary'`.
    #[test]
    fn check_binary_mode_is_keyed_on_exactly_binary() {
        let config = |mode: Option<&str>| {
            let mut value = serde_json::json!({ "protocol": "http", "stubs": [{
                "predicates": [{ "matches": { "body": "x" } }],
                "responses": [{ "is": { "statusCode": 200 } }]
            }] });
            if let Some(mode) = mode {
                value["mode"] = mode.into();
            }
            serde_json::from_value::<ImposterConfig>(value).expect("config decodes")
        };
        assert!(check_binary_mode(&config(Some("binary"))).is_err());
        for mode in [None, Some("text"), Some("Binary"), Some(" binary")] {
            assert_eq!(check_binary_mode(&config(mode)), Ok(()), "{mode:?}");
        }
        let decoded = config(Some("binary"));
        assert_eq!(decoded.mode.as_deref(), Some("binary"));
        let printed = serde_json::to_value(&decoded).expect("serialize");
        assert!(printed.get("mode").is_none(), "never printed: {printed}");
    }

    // ===== Issue #1609: a proxy's client identity =====

    const IDENTITY_CERT: &str = include_str!("../proxy/testdata/client-identity/client.crt");
    const IDENTITY_KEY: &str = include_str!("../proxy/testdata/client-identity/client.key");
    const IDENTITY_KEY_PBES2: &str =
        include_str!("../proxy/testdata/client-identity/client.pbes2.key");
    const OTHER_KEY: &str = include_str!("../proxy/testdata/client-identity/client-rsa.key");

    fn proxy_stub(identity: &serde_json::Value) -> String {
        let mut proxy = serde_json::json!({"to": "https://localhost:1"});
        for (k, v) in identity.as_object().expect("object") {
            proxy[k] = v.clone();
        }
        serde_json::json!({"responses": [{"proxy": proxy}]}).to_string()
    }

    fn the_proxy(stub: &Stub) -> &ProxyResponse {
        match &stub.responses[0] {
            StubResponse::Proxy { proxy, .. } => proxy,
            other => panic!("not a proxy: {other:?}"),
        }
    }

    #[test]
    fn a_proxy_identity_round_trips_as_mountebank_prints_it() {
        let stub: Stub = serde_json::from_str(&proxy_stub(&serde_json::json!({
            "cert": IDENTITY_CERT, "key": IDENTITY_KEY_PBES2, "passphrase": "rift-test",
        })))
        .expect("admitted");
        let printed = serde_json::to_value(&stub).expect("serialize");
        let proxy = &printed["responses"][0]["proxy"];
        assert_eq!(proxy["cert"], IDENTITY_CERT);
        assert_eq!(proxy["key"], IDENTITY_KEY_PBES2);
        assert_eq!(proxy["passphrase"], "rift-test");
        // And it is not printed when absent.
        let plain: Stub = serde_json::from_str(&proxy_stub(&serde_json::json!({}))).expect("ok");
        let printed = serde_json::to_value(&plain).expect("serialize");
        let proxy = printed["responses"][0]["proxy"]
            .as_object()
            .expect("object");
        assert!(
            !proxy.contains_key("cert") && !proxy.contains_key("key"),
            "{proxy:?}"
        );
    }

    #[test]
    fn a_bad_proxy_identity_is_an_admission_refusal_not_a_decode_one() {
        for (identity, refusal) in [
            (
                serde_json::json!({"cert": IDENTITY_CERT}),
                "proxy.cert is given without proxy.key",
            ),
            (
                serde_json::json!({"key": IDENTITY_KEY}),
                "proxy.key is given without proxy.cert",
            ),
            (
                serde_json::json!({"cert": IDENTITY_CERT, "key": OTHER_KEY}),
                "does not pair with proxy.cert",
            ),
            (
                serde_json::json!({"cert": IDENTITY_CERT, "key": IDENTITY_KEY_PBES2}),
                "no proxy.passphrase",
            ),
            (
                serde_json::json!({
                    "cert": IDENTITY_CERT, "key": IDENTITY_KEY_PBES2, "passphrase": "nope",
                }),
                "does not decrypt",
            ),
        ] {
            let json = proxy_stub(&identity);
            let door = serde_json::from_str::<Stub>(&json)
                .expect_err("the door refuses")
                .to_string();
            assert!(door.contains(refusal), "{identity}: {door}");
            assert!(!door.contains("nope"), "the passphrase leaked: {door}");

            let mut deserializer = serde_json::Deserializer::from_str(&json);
            let replayed: Stub =
                deserialize_replayed(&mut deserializer).expect("a replay still decodes it");
            let checked = admission_check_stub(&replayed).expect_err("admission_check refuses");
            assert!(checked.contains(refusal), "{identity}: {checked}");
        }
    }

    #[test]
    fn a_costly_encrypted_key_is_refused_fast_at_admission() {
        // The key states its own KDF cost; a door must refuse it before deriving anything.
        use crate::proxy::client_identity::tests::{pbkdf2_key, scrypt_key};
        for key in [scrypt_key(40, 8, 1), pbkdf2_key(4_000_000_000)] {
            let json = proxy_stub(&serde_json::json!({
                "cert": IDENTITY_CERT, "key": key, "passphrase": "x",
            }));
            let started = std::time::Instant::now();
            let door = serde_json::from_str::<Stub>(&json)
                .expect_err("refused")
                .to_string();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(1),
                "took {:?}: {door}",
                started.elapsed()
            );
            assert!(door.contains("too costly"), "{door}");
        }
    }

    fn many_proxy_stubs(keys: &[String]) -> String {
        let stubs: Vec<_> = keys
            .iter()
            .map(|key| {
                serde_json::json!({"responses": [{"proxy": {
                    "to": "https://localhost:1",
                    "cert": IDENTITY_CERT, "key": key, "passphrase": "rift-test",
                }}]})
            })
            .collect();
        serde_json::json!({"protocol": "http", "stubs": stubs}).to_string()
    }

    #[test]
    fn a_document_repeating_one_identity_derives_it_once() {
        use crate::proxy::client_identity::{ADMISSION_DERIVATIONS, AdmissionBudget};
        let document = many_proxy_stubs(&vec![IDENTITY_KEY_PBES2.to_string(); 100]);
        let before = ADMISSION_DERIVATIONS.with(std::cell::Cell::get);
        let config: Result<ImposterConfig, _> = {
            let _budget = AdmissionBudget::enter();
            serde_json::from_str(&document)
        };
        config.expect("admitted");
        assert_eq!(ADMISSION_DERIVATIONS.with(std::cell::Cell::get) - before, 1);
    }

    #[test]
    fn a_document_is_held_to_one_derivation_budget() {
        use crate::proxy::client_identity::AdmissionBudget;
        use crate::proxy::client_identity::tests::encrypted_key;
        use pkcs8::pkcs5::pbes2::Pbkdf2Prf;
        // Distinct real keys at 2048 iterations each; a budget of 5000 admits two of them.
        let keys: Vec<String> = (1..=100)
            .map(|salt| encrypted_key(2048, salt, Pbkdf2Prf::HmacWithSha256))
            .collect();
        let err = {
            let _budget = AdmissionBudget::enter_with_budget(5000);
            serde_json::from_str::<ImposterConfig>(&many_proxy_stubs(&keys))
                .expect_err("refused")
                .to_string()
        };
        assert!(
            err.contains("more key derivation than one request may"),
            "{err}"
        );
        // Two keys fit, so a two-stub document is admitted under the same budget.
        let _budget = AdmissionBudget::enter_with_budget(5000);
        serde_json::from_str::<ImposterConfig>(&many_proxy_stubs(&keys[..2])).expect("admitted");
    }

    #[test]
    fn the_standard_budget_admits_many_openssl_default_keys() {
        use crate::proxy::client_identity::{AdmissionBudget, MAX_ADMISSION_KDF_UNITS};
        // 2048-iteration keys (OpenSSL's default): hundreds fit in one request.
        const _: () = assert!(MAX_ADMISSION_KDF_UNITS / 2048 > 900);
        let _budget = AdmissionBudget::enter();
        serde_json::from_str::<ImposterConfig>(&many_proxy_stubs(&vec![
            IDENTITY_KEY_PBES2
                .to_string();
            3
        ]))
        .expect("admitted");
    }

    #[test]
    fn a_passphrase_alone_or_empty_strings_present_no_identity() {
        for identity in [
            serde_json::json!({"passphrase": "x"}),
            serde_json::json!({"cert": "", "key": ""}),
        ] {
            let stub: Stub = serde_json::from_str(&proxy_stub(&identity)).expect("admitted");
            assert!(
                the_proxy(&stub).client_identity_pem().is_none(),
                "{identity}"
            );
        }
    }

    #[test]
    fn proxy_debug_never_prints_the_key_or_passphrase() {
        let stub: Stub = serde_json::from_str(&proxy_stub(&serde_json::json!({
            "cert": IDENTITY_CERT, "key": IDENTITY_KEY_PBES2, "passphrase": "rift-test",
        })))
        .expect("admitted");
        let shown = format!("{stub:?}");
        let body_line = IDENTITY_KEY_PBES2.lines().nth(1).expect("a body line");
        assert!(!shown.contains(body_line), "the key leaked: {shown}");
        assert!(
            !shown.contains("rift-test"),
            "the passphrase leaked: {shown}"
        );
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    // ===== Issue #1314: one parse for a proxy's `mode` =====

    #[test]
    fn proxy_mode_parses_once_with_mountebanks_default() {
        use crate::recording::ProxyMode;
        let mode = |m: &str| {
            ProxyResponse {
                mode: m.to_string(),
                ..ProxyResponse::default()
            }
            .mode()
        };
        assert_eq!(mode(""), Ok(ProxyMode::ProxyOnce));
        assert_eq!(mode("proxyOnce"), Ok(ProxyMode::ProxyOnce));
        assert_eq!(mode("proxyAlways"), Ok(ProxyMode::ProxyAlways));
        assert_eq!(mode("proxyTransparent"), Ok(ProxyMode::ProxyTransparent));
        assert_eq!(mode("PROXYALWAYS"), Ok(ProxyMode::ProxyAlways));
        assert_eq!(mode(" proxyOnce "), Ok(ProxyMode::ProxyOnce));
        let err = mode("bogus").expect_err("unknown mode");
        assert!(
            err.contains("bogus") && err.contains("proxyTransparent"),
            "{err}"
        );
    }

    #[test]
    fn an_unknown_proxy_mode_is_refused_at_admission_but_decodes_when_replayed() {
        let raw = serde_json::json!({
            "responses": [{ "proxy": { "to": "http://127.0.0.1:9", "mode": "bogus" } }]
        });
        assert!(
            serde_json::from_value::<Stub>(raw.clone()).is_err(),
            "a config door refuses an unknown mode"
        );
        // rift-cluster replays configs an older engine admitted: they must still decode (#1268).
        let stub: Stub = deserialize_replayed(raw).expect("a replayed stub decodes");
        assert!(
            admission_check_stub(&stub).is_err(),
            "admission re-check refuses it"
        );
    }

    fn imposter_proxying_to(to: &str) -> String {
        serde_json::json!({
            "port": 4545, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": { "to": to } }] }]
        })
        .to_string()
    }

    /// Issue #1550: Mountebank's dry run refuses an `http(s)` `to` whose WHATWG pathname is not
    /// `/`, in these words, on every config door.
    #[test]
    fn a_proxy_to_with_a_path_is_refused_in_mountebanks_words() {
        for (to, path) in [
            ("http://localhost:1/x", "/x"),
            ("https://localhost:1/a/b", "/a/b"),
            ("http://localhost:1/x?q=1", "/x"),
            ("http://localhost:1//", "//"),
            ("HTTP://localhost:1/x", "/x"),
        ] {
            let err = serde_json::from_str::<ImposterConfig>(&imposter_proxying_to(to))
                .expect_err(to)
                .to_string();
            assert!(
                err.contains(&format!("proxy.to must not contain a path '{path}'")),
                "{to}: {err}"
            );
        }
        let err = serde_json::from_str::<ImposterConfig>(&imposter_proxying_to("not a url"))
            .expect_err("not a url")
            .to_string();
        assert!(
            err.contains("proxy.to is not a valid URL: not a url"),
            "{err}"
        );
    }

    /// What Mountebank admits: the pathname is `/` (dot segments removed, as WHATWG does), or the
    /// scheme is not http, or the string parses with a non-http scheme.
    #[test]
    fn a_proxy_to_mountebank_admits_is_admitted() {
        for to in [
            "http://localhost:1",
            "http://localhost:1/",
            "http://localhost:1?q=1",
            "http://localhost:1/#f",
            "http://localhost:1/.",
            "http://localhost:1/%2e/",
            "http://user:pw@localhost:1",
            "tcp://localhost:1/x",
            "localhost:1/x",
        ] {
            serde_json::from_str::<ImposterConfig>(&imposter_proxying_to(to))
                .unwrap_or_else(|e| panic!("{to}: {e}"));
        }
    }

    use super::*;
    use serde_json::json;

    // Issue #1104: `0` is the BSD "any free port" idiom and create auto-assigns it (#637), so every
    // door that asks whether a port was pinned must read `Some(0)` the way it reads `None`.
    #[test]
    fn explicit_port_reads_zero_as_absent() {
        let with = |port: Option<u16>| ImposterConfig {
            port,
            ..Default::default()
        };
        assert_eq!(with(None).explicit_port(), None);
        assert_eq!(with(Some(0)).explicit_port(), None);
        assert_eq!(with(Some(1)).explicit_port(), Some(1));
        assert_eq!(with(Some(4545)).explicit_port(), Some(4545));
        assert_eq!(with(Some(u16::MAX)).explicit_port(), Some(u16::MAX));
    }

    // AC 608-4 (#608): a `_behaviors` block that fails to parse is still dropped — the parse-once
    // cache has nowhere to put it — but it must never be dropped *silently*. Since #1162 no config
    // door reaches this (the parser refuses the block), so it is driven through `new_is` directly:
    // the programmatic path is the one that can still hand it anything.
    #[test]
    #[tracing_test::traced_test]
    fn unparseable_behaviors_are_dropped_loudly() {
        let resp = StubResponse::new_is(
            IsResponse::new(
                200,
                crate::imposter::Headers::new(),
                None,
                ResponseMode::default(),
            ),
            Some(json!({ "wait": { "bogus": true } })),
            None,
        );

        match &resp {
            StubResponse::Is {
                behaviors,
                behaviors_parsed,
                ..
            } => {
                assert!(behaviors.is_some(), "the raw block is retained");
                assert!(behaviors_parsed.is_none(), "but it could not be parsed");
            }
            other => panic!("expected an Is response, got {other:?}"),
        }

        assert!(
            logs_contain("_behaviors"),
            "a dropped behaviors block must be visible in the log, not silent"
        );
    }

    // Issue #1101: `ResponseBehaviors` is a derived named-field struct, so serde also accepts a JSON
    // array and fills its fields by position (wait, repeat, copy, lookup, shellTransform, decorate).
    // The injection gate only reads object keys, so a positional `_behaviors` ran a shell command
    // without --allowInjection. Every shape below must be refused where the stub is parsed, naming
    // the key, so no door can admit it.
    #[test]
    fn a_non_object_underscore_behaviors_is_refused_at_parse() {
        let shapes = [
            json!([null, null, null, null, "echo pwned"]),
            json!([
                500,
                null,
                null,
                null,
                ["echo pwned"],
                "function () { return 1; }"
            ]),
            json!([{ "wait": 500 }]),
            json!([]),
            json!("not-an-object"),
            json!(5),
            json!(true),
        ];
        for shape in shapes {
            for response in [
                json!({ "is": { "statusCode": 200 }, "_behaviors": shape.clone() }),
                // Flat / recorded form (issue #304) reads the same field.
                json!({ "statusCode": 200, "_behaviors": shape.clone() }),
            ] {
                let err = serde_json::from_value::<StubResponse>(response.clone())
                    .expect_err("a non-object `_behaviors` must not construct a response")
                    .to_string();
                assert!(
                    err.contains("`_behaviors`"),
                    "the error must name the key, got {err:?} for {response}"
                );
            }
        }
    }

    #[test]
    fn a_scalar_behaviors_is_refused_at_parse() {
        for shape in [json!("not-an-object"), json!(5), json!(false)] {
            let response = json!({ "is": { "statusCode": 200 }, "behaviors": shape });
            let err = serde_json::from_value::<StubResponse>(response.clone())
                .expect_err("a scalar `behaviors` must not construct a response")
                .to_string();
            assert!(
                err.contains("`behaviors`"),
                "the error must name the key, got {err:?} for {response}"
            );
        }
    }

    /// The value of the first `key` step of a compiled program.
    fn step<'a>(
        behaviors: &'a Option<serde_json::Value>,
        key: &str,
    ) -> Option<&'a serde_json::Value> {
        behaviors
            .as_ref()?
            .as_array()?
            .iter()
            .find_map(|element| element.get(key))
    }

    fn raw_behaviors_of(response: serde_json::Value) -> Option<serde_json::Value> {
        match serde_json::from_value::<StubResponse>(response).expect("response parses") {
            StubResponse::Is { behaviors, .. } => behaviors,
            other => panic!("expected an Is response, got {other:?}"),
        }
    }

    // The refusal must not reach the shapes the engine documents.
    #[test]
    fn the_documented_behaviors_shapes_still_parse() {
        // Stored as the compiled program (issue #1198).
        assert_eq!(
            raw_behaviors_of(json!({ "is": {}, "_behaviors": { "wait": 5 } })),
            Some(json!([{ "wait": 5 }]))
        );
        assert_eq!(
            raw_behaviors_of(json!({ "is": {}, "_behaviors": null, "behaviors": [{ "wait": 5 }] })),
            Some(json!([{ "wait": 5 }]))
        );
        assert_eq!(
            raw_behaviors_of(json!({ "is": {}, "behaviors": { "repeat": 2 } })),
            Some(json!([{ "repeat": 2 }]))
        );
        assert_eq!(
            raw_behaviors_of(json!({ "is": {}, "behaviors": [{ "wait": 1 }, { "wait": 2 }] })),
            Some(json!([{ "wait": 1 }, { "wait": 2 }]))
        );
        assert_eq!(raw_behaviors_of(json!({ "is": {}, "behaviors": [] })), None);
        assert_eq!(
            raw_behaviors_of(json!({ "is": {}, "behaviors": null })),
            None
        );
        assert_eq!(raw_behaviors_of(json!({ "is": {} })), None);
    }

    // `new_is` is also reached programmatically, not only through the parser above. The positional
    // read is the hazard itself, so the executor's cache must never be filled from a non-object.
    #[test]
    #[tracing_test::traced_test]
    fn new_is_never_parses_a_non_object_block_positionally() {
        let resp = StubResponse::new_is(
            IsResponse::new(
                200,
                crate::imposter::Headers::new(),
                None,
                ResponseMode::default(),
            ),
            Some(json!([null, null, null, null, "echo pwned"])),
            None,
        );
        match &resp {
            StubResponse::Is {
                behaviors_parsed, ..
            } => assert!(
                behaviors_parsed.is_none(),
                "a positional array must not become a shellTransform, got {behaviors_parsed:?}"
            ),
            other => panic!("expected an Is response, got {other:?}"),
        }
        assert!(
            logs_contain("_behaviors"),
            "the dropped block must be visible in the log"
        );
    }

    // A well-formed block must not log — the error line means something is wrong.
    #[test]
    #[tracing_test::traced_test]
    fn well_formed_behaviors_do_not_log() {
        let resp: StubResponse = serde_json::from_value(json!({
            "is": { "statusCode": 200 },
            "_behaviors": { "wait": { "inject": "function() { return 5; }" } }
        }))
        .expect("object-form wait parses");
        match &resp {
            StubResponse::Is {
                behaviors_parsed, ..
            } => assert!(
                behaviors_parsed.is_some(),
                "the documented object-form wait must reach the cache"
            ),
            other => panic!("expected an Is response, got {other:?}"),
        }
        assert!(
            !logs_contain("_behaviors"),
            "a valid block must not emit the dropped-behaviors error"
        );
    }

    // Issue #479: `_behaviors` are parsed and a non-string body is rendered ONCE at construction,
    // so the request hot path reads cached values instead of re-deserializing/re-serializing.
    #[test]
    fn is_response_precomputes_behaviors_and_rendered_body() {
        let resp: StubResponse = serde_json::from_value(json!({
            "is": { "statusCode": 200, "body": { "id": 1, "name": "x" } },
            "_behaviors": { "wait": 25 }
        }))
        .unwrap();
        match resp {
            StubResponse::Is {
                behaviors_parsed,
                rendered_body,
                ..
            } => {
                assert!(
                    behaviors_parsed.is_some(),
                    "_behaviors must be parsed once at construction"
                );
                assert_eq!(
                    rendered_body.as_deref(),
                    Some(r#"{"id":1,"name":"x"}"#),
                    "a non-string body must be pre-rendered once at construction"
                );
            }
            other => panic!("expected Is, got {other:?}"),
        }

        // A string body needs no rendering, and no _behaviors means no parsed behaviors.
        let plain: StubResponse =
            serde_json::from_value(json!({ "is": { "statusCode": 200, "body": "hello" } }))
                .unwrap();
        match plain {
            StubResponse::Is {
                behaviors_parsed,
                rendered_body,
                ..
            } => {
                assert!(behaviors_parsed.is_none());
                assert!(rendered_body.is_none(), "a string body is not pre-rendered");
            }
            other => panic!("expected Is, got {other:?}"),
        }
    }

    // Issue #1085: a float in a stub body is served with the digits it was written with. Parsed
    // from text on purpose — `json!` would turn `7e23` into a Rust `f64` literal at compile time,
    // which is correctly rounded and so never exercised serde_json's parser.
    #[test]
    fn a_float_in_an_is_body_is_rendered_with_the_digits_written() {
        let resp: StubResponse = serde_json::from_str(
            r#"{"is": {"statusCode": 200, "body": {"n": 7e23, "m": 0.10018513143495411, "s": 1.23e-30}}}"#,
        )
        .unwrap();
        match resp {
            StubResponse::Is { rendered_body, .. } => assert_eq!(
                rendered_body.as_deref(),
                Some(r#"{"m":0.10018513143495411,"n":7e+23,"s":1.23e-30}"#),
            ),
            other => panic!("expected Is, got {other:?}"),
        }
    }

    // A provider's own options survive under `flowState`, verbatim, instead of
    // being dropped on the floor as unknown keys.
    #[test]
    fn provider_options_collect_in_extra() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({
            "ttlSeconds": 60,
            "flowIdSource": "header:X-Flow",
            "readConsistency": "strong",
            "nested": { "fsyncIntervalMs": 50 }
        }))
        .unwrap();

        // The typed fields still bind; only what is left over collects.
        assert_eq!(fs.ttl_seconds, 60);
        assert_eq!(fs.flow_id_source.as_deref(), Some("header:X-Flow"));
        assert_eq!(
            fs.extra.get("readConsistency").and_then(|v| v.as_str()),
            Some("strong")
        );
        assert_eq!(
            fs.extra
                .get("nested")
                .and_then(|v| v.get("fsyncIntervalMs"))
                .and_then(serde_json::Value::as_u64),
            Some(50)
        );
        assert!(
            !fs.extra.contains_key("flowIdSource") && !fs.extra.contains_key("ttlSeconds"),
            "a key a typed field already claimed must not also land in extra: {:?}",
            fs.extra
        );

        // Round-trip: provider keys come back flat, where they were written.
        let out = serde_json::to_value(&fs).unwrap();
        assert_eq!(
            out.get("readConsistency").and_then(|v| v.as_str()),
            Some("strong")
        );
    }

    // `rename_all = "camelCase"` renames the *declared* fields; keys collected by
    // `flatten` are data, not identifiers, and must pass through untouched.
    #[test]
    fn extra_keys_are_not_case_mangled() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({
            "some_snake_key": 1,
            "SCREAMING": 2,
            "mixedCase": 3
        }))
        .unwrap();
        for key in ["some_snake_key", "SCREAMING", "mixedCase"] {
            assert!(fs.extra.contains_key(key), "{key} was renamed or dropped");
        }
    }

    // Adding `extra` must not change the wire form of any config that exists
    // today: an empty map contributes no keys, so a round-trip of a
    // fully-populated built-in config is byte-identical to what it was before.
    #[test]
    fn an_empty_extra_adds_nothing_to_the_wire_form() {
        let input = json!({
            "ttlSeconds": 900,
            "flowIdSource": "header:X-Flow"
        });
        let fs: RiftFlowStateConfig = serde_json::from_value(input.clone()).unwrap();
        assert!(fs.extra.is_empty(), "nothing was left over: {:?}", fs.extra);

        let out = serde_json::to_value(&fs).unwrap();
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 2, "serializing gained or lost a key: {keys:?}");
        assert_eq!(out.get("ttlSeconds"), input.get("ttlSeconds"));
        assert_eq!(out.get("flowIdSource"), input.get("flowIdSource"));
    }

    // Issues #1337/#1463: the Redis backend and then `backend` itself were removed, but a stored
    // config naming one must still DECODE (rift-cluster replays admitted bytes; a decode-time
    // refusal would make them unreadable). The leftover `backend` key and `redis` block are
    // carried in `extra` and survive a re-serialize unchanged.
    #[test]
    fn a_leftover_redis_backend_and_block_still_decode_and_round_trip() {
        let input = json!({
            "backend": "redis",
            "ttlSeconds": 900,
            "redis": { "url": "redis://localhost:6379", "poolSize": 4 }
        });
        let fs: RiftFlowStateConfig = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(fs.extra.get("backend"), Some(&json!("redis")));
        assert_eq!(
            fs.extra.get("redis"),
            Some(&json!({ "url": "redis://localhost:6379", "poolSize": 4 }))
        );
        assert_eq!(serde_json::to_value(&fs).unwrap(), input);
    }

    // Absent `flowState` keys entirely: `extra` defaults empty rather than
    // failing to deserialize.
    #[test]
    fn extra_defaults_empty() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({})).unwrap();
        assert!(fs.extra.is_empty());
        assert_eq!(fs.ttl_seconds, 300);
        assert!(RiftFlowStateConfig::default().extra.is_empty());
    }

    // Issue #266: `flowIdSource` is flat under `flowState` and survives a serialize round-trip.
    #[test]
    fn flat_flow_id_source_round_trips() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({
            "flowIdSource": "header:X-Mock-Space"
        }))
        .unwrap();
        assert_eq!(fs.flow_id_source.as_deref(), Some("header:X-Mock-Space"));
        let out = serde_json::to_value(&fs).unwrap();
        assert_eq!(
            out.get("flowIdSource").and_then(|v| v.as_str()),
            Some("header:X-Mock-Space")
        );
    }

    // Issue #356: `code`/`file`/`ref` all deserialize into the right field; `engine` stays
    // optional (resolution — not deserialize — fills it in).
    #[test]
    fn script_config_deserializes_code_file_and_ref_variants() {
        let code: RiftScriptConfig =
            serde_json::from_value(json!({ "code": "fn respond() {}" })).unwrap();
        assert_eq!(code.code.as_deref(), Some("fn respond() {}"));
        assert_eq!(code.file, None);
        assert_eq!(code.ref_name, None);
        assert_eq!(
            code.engine, None,
            "engine is not defaulted at deserialize time"
        );

        let file: RiftScriptConfig =
            serde_json::from_value(json!({ "file": "scripts/a.rhai" })).unwrap();
        assert_eq!(file.file.as_deref(), Some("scripts/a.rhai"));
        assert_eq!(file.code, None);

        let by_ref: RiftScriptConfig =
            serde_json::from_value(json!({ "ref": "failTwice" })).unwrap();
        assert_eq!(by_ref.ref_name.as_deref(), Some("failTwice"));
        assert_eq!(by_ref.code, None);
    }

    // Issue #356: back-compat — a pre-existing `{ "engine": ..., "code": ... }` config (the shape
    // before `file`/`ref` existed) still deserializes unchanged.
    #[test]
    fn script_config_back_compat_engine_and_code() {
        let cfg: RiftScriptConfig = serde_json::from_value(json!({
            "engine": "rhai",
            "code": "fn respond() {}"
        }))
        .unwrap();
        assert_eq!(cfg.engine.as_deref(), Some("rhai"));
        assert_eq!(cfg.code.as_deref(), Some("fn respond() {}"));
        assert!(cfg.has_valid_source());
    }

    // Issue #356: exactly one of `code`/`file`/`ref` is valid; zero or multiple is not.
    #[test]
    fn script_config_source_count_invariant() {
        let none = RiftScriptConfig::default();
        assert_eq!(none.source_count(), 0);
        assert!(!none.has_valid_source());

        let one = RiftScriptConfig {
            code: Some("x".to_string()),
            ..Default::default()
        };
        assert!(one.has_valid_source());

        let two = RiftScriptConfig {
            code: Some("x".to_string()),
            file: Some("y.rhai".to_string()),
            ..Default::default()
        };
        assert_eq!(two.source_count(), 2);
        assert!(!two.has_valid_source());
    }

    // Issue #356: `_rift.scripts` registry round-trips and defaults to empty/omitted.
    #[test]
    fn rift_config_scripts_registry_round_trips() {
        let cfg: RiftConfig = serde_json::from_value(json!({
            "scripts": { "failTwice": { "file": "fail-twice.rhai" } }
        }))
        .unwrap();
        assert_eq!(
            cfg.scripts.get("failTwice").and_then(|s| s.file.as_deref()),
            Some("fail-twice.rhai")
        );

        let empty = RiftConfig::default();
        assert!(empty.scripts.is_empty());
        let out = serde_json::to_value(&empty).unwrap();
        assert!(
            out.get("scripts").is_none(),
            "empty registry must be omitted on serialize"
        );
    }

    // The `_rift.sequencing` block is carried for a provider-supplied `ResponseSequencer`:
    // declared so it survives the parse (`_rift` has no catch-all), unknown keys collected
    // rather than dropped, and absent unless the author wrote one.
    #[test]
    fn rift_config_sequencing_block_is_carried_with_its_extra_keys() {
        let cfg: RiftConfig = serde_json::from_value(json!({
            "sequencing": { "mode": "owner", "peekCacheMs": 50 }
        }))
        .unwrap();
        let sequencing = cfg
            .sequencing
            .as_ref()
            .expect("the block must survive the parse");
        assert_eq!(sequencing.mode.as_deref(), Some("owner"));
        assert_eq!(
            sequencing.extra.get("peekCacheMs"),
            Some(&json!(50)),
            "a key this crate does not know belongs to the sequencer, not in the bin"
        );

        // Round-trips: an embedder reading the config back out sees what was written.
        let out = serde_json::to_value(&cfg).unwrap();
        assert_eq!(out["sequencing"]["mode"], json!("owner"));
        assert_eq!(out["sequencing"]["peekCacheMs"], json!(50));

        let empty = RiftConfig::default();
        assert!(empty.sequencing.is_none());
        assert!(
            serde_json::to_value(&empty)
                .unwrap()
                .get("sequencing")
                .is_none(),
            "an imposter that said nothing about sequencing must not grow a block"
        );
    }

    // Issue #266: an absent `flowIdSource` deserializes to None and is omitted on serialize.
    #[test]
    fn absent_flow_id_source_is_none_and_omitted() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({ "ttlSeconds": 60 })).unwrap();
        assert_eq!(fs.flow_id_source, None);
        let out = serde_json::to_value(&fs).unwrap();
        assert!(out.get("flowIdSource").is_none());
    }

    // Issue #251: an engine-ignored `_verify` annotation on a stub must survive the load
    // round-trip so rift-verify can read it back from GET /imposters.
    #[test]
    fn stub_preserves_verify_passthrough() {
        let spec = json!({ "sequence": [
            { "request": { "method": "GET", "path": "/r" }, "expect": { "status": 503 } }
        ]});
        let stub: Stub = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/r" } }],
            "responses": [{ "is": { "statusCode": 503 } }],
            "_verify": spec.clone(),
        }))
        .unwrap();
        assert_eq!(stub.verify.as_ref(), Some(&spec));
        let out = serde_json::to_value(&stub).unwrap();
        assert_eq!(out.get("_verify"), Some(&spec));
    }

    // Issue #238: multi-value headers accept "k":"v" and "k":["v1","v2"]; serialize back as
    // a string for one value and an array for many (via IsResponse, which uses the helper).
    // Issue #977: `ca` mirrors Mountebank's shape — one PEM or a list of them.
    // (The issue-#238 note below belongs to `multi_value_headers_deserialize_string_or_array`.)
    #[test]
    fn ca_accepts_a_string_or_an_array() {
        let one: ImposterConfig =
            serde_json::from_value(serde_json::json!({"protocol": "https", "ca": "PEM-A"}))
                .expect("a bare string parses");
        assert_eq!(one.ca, Some(PemList::One("PEM-A".to_string())));

        let many: ImposterConfig = serde_json::from_value(
            serde_json::json!({"protocol": "https", "ca": ["PEM-A", "PEM-B"]}),
        )
        .expect("an array parses");
        assert_eq!(
            many.ca.as_ref().map(PemList::anchors),
            Some(["PEM-A".to_string(), "PEM-B".to_string()].as_slice())
        );

        let absent: ImposterConfig =
            serde_json::from_value(serde_json::json!({"protocol": "https"}))
                .expect("absent parses");
        assert!(absent.ca.is_none());
    }

    // The error has to be actionable at the POST /imposters boundary — an untagged enum would say
    // only "data did not match any variant", which is why this deserializer is hand-written.
    #[test]
    fn ca_rejects_a_non_string_with_an_actionable_message() {
        let err = serde_json::from_value::<ImposterConfig>(
            serde_json::json!({"protocol": "https", "ca": 42}),
        )
        .expect_err("a number is not a PEM");
        assert!(
            err.to_string().contains("PEM string"),
            "message must say what was expected, got: {err}"
        );

        let err = serde_json::from_value::<ImposterConfig>(
            serde_json::json!({"protocol": "https", "ca": ["PEM-A", 7]}),
        )
        .expect_err("a non-string array entry is not a PEM");
        assert!(
            err.to_string().contains("array entries"),
            "message must point at the entry, got: {err}"
        );
    }

    // Wire compatibility: an imposter that never mentions client auth must serialize exactly as it
    // did before #977, or every SDK's parse-fidelity gate breaks on a field it has never seen.
    // This is the stored form (`--datadir`, rift-cluster's `PutImposter` bytes); the printed https
    // views add the flags and `ca: null` on top of it (issue #1576), the stored form does not.
    #[test]
    fn client_auth_defaults_are_not_serialized() {
        let cfg: ImposterConfig =
            serde_json::from_value(serde_json::json!({"port": 4545, "protocol": "http"}))
                .expect("config");
        let out = serde_json::to_value(&cfg).expect("serialize");
        let obj = out.as_object().expect("object");
        for k in ["mutualAuth", "rejectUnauthorized", "ca"] {
            assert!(!obj.contains_key(k), "`{k}` must be omitted when unset");
        }
    }

    // ...but once set they round-trip under their camelCase names.
    #[test]
    fn client_auth_round_trips_under_camel_case() {
        let cfg: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "https", "mutualAuth": true, "rejectUnauthorized": true, "ca": "PEM"
        }))
        .expect("config");
        assert!(cfg.mutual_auth && cfg.reject_unauthorized);
        let out = serde_json::to_value(&cfg).expect("serialize");
        assert_eq!(out["mutualAuth"], serde_json::json!(true));
        assert_eq!(out["rejectUnauthorized"], serde_json::json!(true));
        // One anchor round-trips as the bare string it arrived as (issue #238's precedent).
        assert_eq!(out["ca"], serde_json::json!("PEM"));
    }

    // Issue #1576: the stored form (the `--datadir` file, rift-cluster's committed `PutImposter`
    // bytes) of an https imposter without client auth carries no client-auth keys either. The
    // admin views print them (`ca: null` included); the stored form must not.
    #[test]
    fn https_client_auth_defaults_are_not_stored() {
        let cfg: ImposterConfig =
            serde_json::from_value(serde_json::json!({"port": 4545, "protocol": "https"}))
                .expect("config");
        let out = serde_json::to_value(&cfg).expect("serialize");
        let obj = out.as_object().expect("object");
        for k in ["mutualAuth", "rejectUnauthorized", "ca", "ciphers"] {
            assert!(!obj.contains_key(k), "`{k}` must be omitted when unset");
        }
    }

    // Issue #1576: `ca` keeps the spelling it arrived in — a one-element array stays an array, as
    // Mountebank echoes it — and a bare string stays a string.
    #[test]
    fn ca_keeps_the_spelling_it_arrived_in() {
        for ca in [serde_json::json!(["PEM"]), serde_json::json!("PEM")] {
            let cfg: ImposterConfig = serde_json::from_value(serde_json::json!({
                "protocol": "https", "mutualAuth": true, "rejectUnauthorized": true, "ca": ca
            }))
            .expect("config");
            let out = serde_json::to_value(&cfg).expect("serialize");
            assert_eq!(out["ca"], ca);
        }
    }

    // Issue #1576: `ciphers` is stored verbatim — never upper-cased at decode — so it survives a
    // restart and a replayable export instead of being silently dropped.
    #[test]
    fn ciphers_is_stored_verbatim() {
        let cfg: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "https", "ciphers": "ecdhe-rsa-aes128-gcm-sha256"
        }))
        .expect("config");
        let out = serde_json::to_value(&cfg).expect("serialize");
        assert_eq!(
            out["ciphers"],
            serde_json::json!("ecdhe-rsa-aes128-gcm-sha256")
        );
    }

    // Issue #1050: the wiring half. The algorithm is covered in `wire.rs`; what these pin is that
    // the `deserialize_with` attribute is actually ON these two fields — the failure mode a
    // per-site test exists for, and the reason #1049's own review asked for one.
    #[test]
    fn proxy_inject_headers_rejects_a_name_given_twice() {
        let err = serde_json::from_str::<ProxyResponse>(
            r#"{"to":"http://x","injectHeaders":{"x-trace":"a","X-Trace":"b"}}"#,
        )
        .expect_err("injectHeaders names each header once");
        assert!(
            err.to_string().contains("names each header once"),
            "unexpected error: {err}"
        );

        let ok: ProxyResponse =
            serde_json::from_str(r#"{"to":"http://x","injectHeaders":{"x-trace":"a"}}"#)
                .expect("a single spelling is unaffected");
        assert_eq!(ok.inject_headers["x-trace"], "a");
    }

    #[test]
    fn fault_error_headers_reject_a_name_given_twice() {
        let err = serde_json::from_str::<RiftErrorFault>(
            r#"{"status":503,"headers":{"retry-after":"1","Retry-After":"2"}}"#,
        )
        .expect_err("fault error headers name each header once");
        assert!(err.to_string().contains("names each header once"), "{err}");

        let ok: RiftErrorFault =
            serde_json::from_str(r#"{"status":503,"headers":{"Retry-After":"1"}}"#)
                .expect("a single spelling is unaffected");
        assert_eq!(ok.headers["Retry-After"], "1");
    }

    #[test]
    fn multi_value_headers_deserialize_string_or_array() {
        let single: IsResponse =
            serde_json::from_str(r#"{"statusCode":200,"headers":{"X-One":"v"}}"#).unwrap();
        assert_eq!(single.headers["X-One"], vec!["v".to_string()]);

        let many: IsResponse =
            serde_json::from_str(r#"{"statusCode":200,"headers":{"Set-Cookie":["a","b"]}}"#)
                .unwrap();
        assert_eq!(
            many.headers["Set-Cookie"],
            vec!["a".to_string(), "b".to_string()]
        );
    }

    // Issue #754: Mountebank tolerates non-string scalar header values (e.g. `"Content-Length":124`,
    // `"X-Flag":true`) and coerces them to strings; Rift previously 400'd the whole imposter. These
    // must deserialize and coerce to their string form, singly and inside arrays.
    #[test]
    fn multi_value_headers_coerce_numeric_and_bool_values() {
        let r: IsResponse = serde_json::from_str(
            r#"{"statusCode":200,"headers":{"Content-Length":124,"X-Flag":true,"X-Ratio":1.5}}"#,
        )
        .expect("numeric/bool header values must be accepted (mb parity)");
        assert_eq!(r.headers["Content-Length"], vec!["124".to_string()]);
        assert_eq!(r.headers["X-Flag"], vec!["true".to_string()]);
        assert_eq!(r.headers["X-Ratio"], vec!["1.5".to_string()]);

        // Arrays may mix scalar kinds too.
        let mixed: IsResponse =
            serde_json::from_str(r#"{"statusCode":200,"headers":{"X-Multi":[200,"x",false]}}"#)
                .unwrap();
        assert_eq!(
            mixed.headers["X-Multi"],
            vec!["200".to_string(), "x".to_string(), "false".to_string()]
        );
    }

    // Issue #1631: a number in a predicate or body is a `serde_json::Value` f64 and prints from
    // its parsed value (`1.0` stays `1.0`, `1e2` becomes `100.0`), where Mountebank prints `1` and
    // `100`. Documented in differences.md; changing it would move `stub_key` for such stubs, so
    // pin it to make an accidental change visible.
    #[test]
    fn printed_predicate_number_keeps_serde_spelling() {
        let stub: Stub = serde_json::from_str(
            r#"{"predicates":[{"equals":{"body":1.0}}],"responses":[{"is":{"body":1e2}}]}"#,
        )
        .expect("a stub with numeric predicate and body values parses");
        let printed = serde_json::to_string(&stub).expect("a parsed stub serializes");
        assert!(
            printed.contains(r#"{"equals":{"body":1.0}}"#),
            "predicate number must print as 1.0: {printed}"
        );
        assert!(
            printed.contains(r#""body":100.0"#),
            "body number 1e2 must print as 100.0: {printed}"
        );
    }

    // Issue #1039: the same fold-and-merge invariant, observed through the two types that
    // actually reach a user — a recorded request and a stub response — rather than only through
    // the deserializer's own test harness.
    #[test]
    fn recorded_request_folds_case_variant_header_keys_and_parses_forms_deterministically() {
        const DOC: &str = r#"{
            "requestFrom": "127.0.0.1:1",
            "method": "POST",
            "path": "/f",
            "query": {},
            "headers": {
                "content-type": "application/x-www-form-urlencoded",
                "Content-Type": "text/plain"
            },
            "body": "a=1&b=2",
            "timestamp": "2024-01-15T10:30:00.000Z"
        }"#;

        // The symptom: two entries for one name made `parse_form_data`'s case-insensitive
        // find-first visit them in unspecified `HashMap` order, so the same stored request
        // answered `/verify` differently across runs. Reconstruct it many times: the surviving
        // key, and therefore the form, must be the same every time.
        for _ in 0..200 {
            let req: RecordedRequest = serde_json::from_str(DOC).expect("valid recording");
            assert_eq!(
                req.headers.len(),
                1,
                "two spellings of Content-Type is one header"
            );
            assert_eq!(
                req.headers["content-type"],
                vec![
                    "application/x-www-form-urlencoded".to_string(),
                    "text/plain".to_string()
                ],
                "first spelling wins, values in document order"
            );
            let form =
                crate::imposter::Imposter::parse_form_data(&req.headers, req.body.as_deref())
                    .expect("the first Content-Type value is the form type, so the body parses");
            assert_eq!(
                form.get("a").and_then(|v| v.first()).map(String::as_str),
                Some("1")
            );
            assert_eq!(
                form.get("b").and_then(|v| v.first()).map(String::as_str),
                Some("2")
            );
        }
    }

    // The fourth site wired to the shared deserializer, and the one the tests above do not reach:
    // the flat/recorded response form (#304), where `headers` sits at the top level with no `is`
    // wrapper. The algorithm is covered in `wire.rs`; what this pins is the `deserialize_with`
    // attribute actually being on this field, which is the failure mode a per-site test exists for.
    #[test]
    fn flat_recorded_response_form_folds_case_variant_header_keys() {
        let stub: StubResponse = serde_json::from_str(
            r#"{"statusCode":200,"headers":{"x-trace":"a","X-Trace":"b"},"body":"ok"}"#,
        )
        .expect("the flat #304 form accepts case-variant keys");
        let StubResponse::Is { is, .. } = stub else {
            panic!("the flat form is normalised into the `is` variant");
        };
        assert_eq!(is.headers.len(), 1, "one name, however it was spelled");
        assert_eq!(
            is.headers["x-trace"],
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn is_response_folds_case_variant_header_keys_and_round_trips_one_key() {
        let r: IsResponse = serde_json::from_str(
            r#"{"statusCode":200,"headers":{"Set-Cookie":"a","set-cookie":["b","c"]}}"#,
        )
        .expect("case-variant response header keys are valid, not an error");
        assert_eq!(r.headers.len(), 1);
        assert_eq!(
            r.headers["Set-Cookie"],
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "values are served in document order under the first spelling"
        );

        let out = serde_json::to_value(&r).expect("serialize");
        assert_eq!(out["headers"]["Set-Cookie"], json!(["a", "b", "c"]));
        assert!(
            out["headers"].get("set-cookie").is_none(),
            "GET /imposters/{{port}} echoes one key, not two"
        );
    }

    #[test]
    fn multi_value_headers_serialize_single_as_string_many_as_array() {
        let mut headers = crate::imposter::Headers::new();
        headers.insert("X-One".to_string(), vec!["v".to_string()]);
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        let out = IsResponseOut::from(IsResponse::new(200, headers, None, ResponseMode::Text));
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(
            v["headers"]["X-One"],
            json!("v"),
            "one value -> bare string"
        );
        assert_eq!(
            v["headers"]["Set-Cookie"],
            json!(["a", "b"]),
            "multiple values -> array"
        );
    }

    // Fix #107: Stub.responses now has #[serde(default)]
    #[test]
    fn flat_response_without_is_wrapper_parses_as_is_string_body() {
        // Issue #304: a flat/recorded response (statusCode/headers/body at top level, no `is`)
        // must be served like `is: { … }`, not as an empty response.
        let r: StubResponse = serde_json::from_value(json!({
            "statusCode": 200,
            "headers": { "Content-Type": "application/json" },
            "body": "FLAT-BODY"
        }))
        .unwrap();
        match r {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.status_code, 200);
                assert_eq!(is.body, Some(json!("FLAT-BODY")));
                assert_eq!(
                    is.headers.get("Content-Type"),
                    Some(&vec!["application/json".to_string()])
                );
            }
            other => panic!("expected Is, got {other:?}"),
        }
    }

    #[test]
    fn flat_response_object_body_parses_as_is() {
        let r: StubResponse = serde_json::from_value(json!({
            "statusCode": 200,
            "body": { "k": "objbody" }
        }))
        .unwrap();
        match r {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.status_code, 200);
                assert_eq!(is.body, Some(json!({ "k": "objbody" })));
            }
            other => panic!("expected Is, got {other:?}"),
        }
    }

    #[test]
    fn flat_response_status_code_as_string_and_body_only() {
        let r: StubResponse =
            serde_json::from_value(json!({ "statusCode": "201", "body": "x" })).unwrap();
        match r {
            StubResponse::Is { is, .. } => assert_eq!(is.status_code, 201),
            other => panic!("expected Is, got {other:?}"),
        }
        // Body-only flat response defaults the status to 200.
        let r: StubResponse = serde_json::from_value(json!({ "body": "just-body" })).unwrap();
        match r {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.status_code, 200);
                assert_eq!(is.body, Some(json!("just-body")));
            }
            other => panic!("expected Is, got {other:?}"),
        }
    }

    #[test]
    fn empty_response_object_still_defaults_to_empty_is() {
        // Regression guard: a genuinely-empty `{}` response a pre-#1551 engine stored must still
        // replay as a 200 empty Is. A config door refuses it now (`unrecognized response type`).
        let r: StubResponse =
            crate::imposter::deserialize_replayed(json!({})).expect("replay admits `{}`");
        match r {
            StubResponse::Is { is, .. } => {
                assert_eq!(is.status_code, 200);
                assert_eq!(is.body, None);
            }
            other => panic!("expected Is, got {other:?}"),
        }
    }

    #[test]
    fn test_stub_deserialize_without_responses_field() {
        // Issue #1372: Mountebank refuses an absent and an empty `responses` alike.
        for stub_json in [
            json!({ "predicates": [{ "equals": { "path": "/test" } }] }),
            json!({ "predicates": [{ "equals": { "path": "/test" } }], "responses": [] }),
        ] {
            let err = serde_json::from_value::<Stub>(stub_json)
                .expect_err("a stub without responses is refused at admission")
                .to_string();
            assert!(
                err.contains("'responses' must be a non-empty array"),
                "{err}"
            );
        }
    }

    #[test]
    fn test_stub_rules_alias_for_predicates() {
        let stub_json = json!({
            "rules": [{ "equals": { "path": "/test" } }],
            "responses": [{ "is": { "statusCode": 200 } }]
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        assert_eq!(stub.predicates.len(), 1);
    }

    /// #1341: the differential harness found replayable exports spelling Mountebank's `allowCORS`
    /// as `allowCors`. It is written as Mountebank spells it, and both spellings still read.
    #[test]
    fn allow_cors_is_written_as_mountebank_spells_it_and_reads_either_spelling() {
        for spelling in ["allowCORS", "allowCors"] {
            let config: ImposterConfig =
                serde_json::from_value(json!({"protocol": "http", spelling: true}))
                    .expect("imposter parses");
            assert!(config.allow_cors.enabled(), "{spelling} is read");
            let written = serde_json::to_value(&config).expect("imposter serialises");
            assert_eq!(written.get("allowCORS"), Some(&json!(true)), "{spelling}");
            assert_eq!(written.get("allowCors"), None, "{spelling}");
        }
    }

    /// #1613: Mountebank never prints `recordMatches`, and the house rule is that a default is not
    /// printed. Absent and `false` print no key; `true` (a Rift-only key that round-trips) prints;
    /// `recordRequests`, which Mountebank always prints, stays.
    #[test]
    fn record_matches_prints_only_when_true() {
        for input in [
            json!({"protocol": "http"}),
            json!({"protocol": "http", "recordMatches": false}),
        ] {
            let config: ImposterConfig = serde_json::from_value(input).expect("imposter parses");
            let written = serde_json::to_value(&config).expect("imposter serialises");
            assert_eq!(written.get("recordMatches"), None, "{written}");
            assert_eq!(
                written.get("recordRequests"),
                Some(&json!(false)),
                "{written}"
            );
            let again: ImposterConfig = serde_json::from_value(written).expect("re-parses");
            assert!(!again.record_matches);
        }
        let config: ImposterConfig =
            serde_json::from_value(json!({"protocol": "http", "recordMatches": true}))
                .expect("imposter parses");
        let written = serde_json::to_value(&config).expect("imposter serialises");
        assert_eq!(
            written.get("recordMatches"),
            Some(&json!(true)),
            "{written}"
        );
        let again: ImposterConfig = serde_json::from_value(written).expect("re-parses");
        assert!(again.record_matches);
    }

    fn allow_cors_of(value: &serde_json::Value) -> ImposterConfig {
        serde_json::from_value(json!({"protocol": "http", "allowCORS": value}))
            .unwrap_or_else(|e| panic!("allowCORS {value} parses: {e}"))
    }

    /// #1453: Mountebank reads `allowCORS` by JavaScript truthiness (`if (options.allowCORS)`), so
    /// any JSON value is accepted, and only `false`, `0`, `""` and `null` leave CORS off.
    #[test]
    fn allow_cors_is_enabled_by_javascript_truthiness() {
        for value in [json!(true), json!("yes"), json!(1), json!({}), json!([])] {
            assert!(
                allow_cors_of(&value).allow_cors.enabled(),
                "{value} enables"
            );
        }
        for value in [json!(false), json!(0), json!(""), json!(null)] {
            assert!(
                !allow_cors_of(&value).allow_cors.enabled(),
                "{value} does not"
            );
        }
        let absent: ImposterConfig =
            serde_json::from_value(json!({"protocol": "http"})).expect("parses");
        assert!(!absent.allow_cors.enabled());
    }

    /// #1453: an enabled value is written back exactly as configured, as Mountebank 2.9.2+ prints
    /// it; a falsy one is omitted. A stored `true` keeps its bytes, `"allowCORS": true`.
    #[test]
    fn allow_cors_round_trips_verbatim_when_enabled_and_is_omitted_otherwise() {
        for value in [json!(true), json!("yes"), json!(1), json!({}), json!([])] {
            let written = serde_json::to_value(allow_cors_of(&value)).expect("serialises");
            assert_eq!(written.get("allowCORS"), Some(&value), "{value}");
            let reread: ImposterConfig = serde_json::from_value(written).expect("re-reads");
            assert!(reread.allow_cors.enabled(), "{value} re-reads enabled");
        }
        for value in [json!(false), json!(0), json!(""), json!(null)] {
            let written = serde_json::to_value(allow_cors_of(&value)).expect("serialises");
            assert_eq!(written.get("allowCORS"), None, "{value}");
        }
        let stored = serde_json::to_string(&allow_cors_of(&json!(true))).expect("serialises");
        assert!(stored.contains(r#""allowCORS":true"#), "{stored}");
    }

    #[test]
    fn allow_cors_alias_reads_any_value() {
        let config: ImposterConfig =
            serde_json::from_value(json!({"protocol": "http", "allowCors": "yes"}))
                .expect("parses");
        assert_eq!(config.allow_cors.value(), Some(&json!("yes")));
    }

    #[test]
    fn test_stub_predicates_takes_precedence_over_rules() {
        let stub_json = json!({
            "predicates": [{ "equals": { "path": "/a" } }],
            "rules": [{ "equals": { "path": "/b" } }, { "equals": { "path": "/c" } }],
            "responses": [{ "is": {} }]
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        assert_eq!(stub.predicates.len(), 1);
    }

    #[test]
    fn test_stub_delay_range_injected_as_wait() {
        let stub_json = json!({
            "predicates": [],
            "delayRange": [{ "min": "50", "max": "100" }],
            "responses": [{ "is": { "statusCode": 200 } }]
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        assert_eq!(stub.responses.len(), 1);
        if let StubResponse::Is { behaviors, .. } = &stub.responses[0] {
            let wait = step(behaviors, "wait").expect("a wait step");
            // min != max → range object
            assert_eq!(wait.get("min").unwrap(), &json!(50u64));
            assert_eq!(wait.get("max").unwrap(), &json!(100u64));
        } else {
            panic!("expected Is response");
        }
    }

    /// Issue #1093: a response's `"wait": null` is no wait, so the stub-level delay fills it.
    #[test]
    fn test_stub_delay_range_fills_a_null_wait() {
        let stub: Stub = serde_json::from_value(json!({
            "predicates": [],
            "delayRange": [{ "min": 200, "max": 200 }],
            "responses": [{ "is": { "statusCode": 200 }, "_behaviors": { "wait": null } }]
        }))
        .unwrap();
        if let StubResponse::Is { behaviors, .. } = &stub.responses[0] {
            assert_eq!(step(behaviors, "wait"), Some(&json!(200u64)));
        } else {
            panic!("expected Is response");
        }
    }

    #[test]
    fn test_stub_delay_range_fixed_when_min_equals_max() {
        let stub_json = json!({
            "predicates": [],
            "delayRange": [{ "min": 0, "max": 0 }],
            "responses": [{ "is": { "statusCode": 200 } }]
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        if let StubResponse::Is { behaviors, .. } = &stub.responses[0] {
            let wait = step(behaviors, "wait").expect("a wait step");
            assert_eq!(wait, &json!(0u64));
        } else {
            panic!("expected Is response");
        }
    }

    // Issue #1148: an inverted range has no delay to draw from, and the draw used to panic the
    // worker on every request to the stub. Refused at the parse door — the #1109 layer — so every
    // entry point (admin API, --configfile, --datadir, reload, FFI) reports it without per-door
    // code. `delayRange` is a second spelling of the same thing and is rewritten into a `wait`
    // after parse, so it needs its own check.
    #[test]
    fn an_inverted_delay_range_is_refused() {
        let stub_json = json!({
            "predicates": [],
            "delayRange": [{ "min": 100, "max": 7 }],
            "responses": [{ "is": { "statusCode": 200 } }]
        });
        let err = serde_json::from_value::<Stub>(stub_json)
            .expect_err("an inverted delayRange must not deserialize");
        let msg = err.to_string();
        // 7, not 10: "10" is a substring of "100", so a `contains("100") && contains("10")`
        // assertion is satisfied by a message naming only `min`, and would pass on half a fix.
        assert!(
            msg.contains("100") && msg.contains('7'),
            "the refusal names both bounds: {msg}"
        );
    }

    // Only the first entry is used, but an inverted range is wrong wherever it sits.
    #[test]
    fn an_inverted_delay_range_is_refused_in_a_later_entry() {
        let stub_json = json!({
            "predicates": [],
            "delayRange": [{ "min": 1, "max": 2 }, { "min": 100, "max": 10 }],
            "responses": [{ "is": { "statusCode": 200 } }]
        });
        assert!(
            serde_json::from_value::<Stub>(stub_json).is_err(),
            "a later inverted delayRange entry must be refused too"
        );
    }

    #[test]
    fn an_inverted_underscore_behaviors_wait_is_refused() {
        let stub_json = json!({
            "predicates": [],
            "responses": [{ "is": { "statusCode": 200 },
                            "_behaviors": { "wait": { "min": 100, "max": 7 } } }]
        });
        let err = serde_json::from_value::<Stub>(stub_json)
            .expect_err("an inverted _behaviors.wait must not deserialize");
        let msg = err.to_string();
        assert!(
            msg.contains("100") && msg.contains('7'),
            "the refusal names both bounds: {msg}"
        );
    }

    #[test]
    fn an_inverted_behaviors_object_wait_is_refused() {
        let stub_json = json!({
            "predicates": [],
            "responses": [{ "is": { "statusCode": 200 },
                            "behaviors": { "wait": { "min": 100, "max": 10 } } }]
        });
        assert!(serde_json::from_value::<Stub>(stub_json).is_err());
    }

    // A later `null` can remove an element's wait from the compiled program, so an inverted range
    // there is the case a check on the compiled program alone would miss.
    #[test]
    fn an_inverted_behaviors_array_wait_is_refused_in_a_non_last_element() {
        let stub_json = json!({
            "predicates": [],
            "responses": [{ "is": { "statusCode": 200 },
                            "behaviors": [{ "wait": { "min": 100, "max": 10 } },
                                          { "wait": 50 }] }]
        });
        assert!(
            serde_json::from_value::<Stub>(stub_json).is_err(),
            "an inverted range must be refused wherever it sits in the array"
        );
    }

    // Equal bounds are a valid inclusive range and must keep working — this is the form
    // `delayRange` with min == max already produces.
    #[test]
    fn an_equal_bound_wait_range_still_parses() {
        let stub_json = json!({
            "predicates": [],
            "responses": [{ "is": { "statusCode": 200 },
                            "_behaviors": { "wait": { "min": 250, "max": 250 } } }]
        });
        let stub: Stub = serde_json::from_value(stub_json).expect("equal bounds are valid");
        let StubResponse::Is { behaviors, .. } = &stub.responses[0] else {
            panic!("expected Is response");
        };
        assert_eq!(
            step(behaviors, "wait"),
            Some(&json!({ "min": 250, "max": 250 }))
        );
    }

    fn response_error(response: serde_json::Value) -> String {
        serde_json::from_value::<StubResponse>(response.clone())
            .expect_err(&format!("must not deserialize: {response}"))
            .to_string()
    }

    // Issue #1162: a behaviors block the executor cannot parse used to be admitted and served with
    // everything but `repeat` ignored. It is refused at parse now, and the refusal names the key
    // the author wrote rather than serde's "did not match any variant of untagged enum".
    #[test]
    fn an_unparseable_wait_is_refused_naming_the_key() {
        for wait in [
            json!({ "min": "100", "max": "200" }),
            json!(500.5),
            json!(-1),
            json!({ "min": 1 }),
            json!({ "bogus": true }),
            json!(true),
            json!([]),
        ] {
            let msg = response_error(json!({
                "is": { "statusCode": 200 },
                "_behaviors": { "wait": wait, "repeat": 2 }
            }));
            assert!(msg.contains("`wait` must be"), "wait {wait}: {msg}");
            assert!(
                !msg.contains("untagged enum"),
                "the message names the key, not serde's enum: {msg}"
            );
        }
    }

    #[test]
    fn an_unparseable_non_wait_key_is_refused_naming_the_key() {
        for (block, key) in [
            (json!({ "repeat": 2.0 }), "`repeat`"),
            (json!({ "repeat": 4_294_967_296_u64 }), "`repeat`"),
            (
                json!({ "copy": [{ "from": "path", "into": "${P}" }] }),
                "`copy`",
            ),
            (
                json!({ "copy": { "from": "path", "into": "${P}" } }),
                "`copy`",
            ),
            (json!({ "lookup": { "into": "${R}" } }), "`lookup`"),
            (json!({ "decorate": 5 }), "`decorate`"),
            (json!({ "shellTransform": [1] }), "`shellTransform`"),
        ] {
            let msg = response_error(json!({ "is": { "statusCode": 200 }, "_behaviors": block }));
            assert!(msg.contains(key), "{block}: {msg}");
        }
    }

    // Every spelling that reaches `new_is` is checked: `behaviors` as an object, as an array (after
    // the merge), and the flat response form with no `is` wrapper.
    #[test]
    fn an_unparseable_block_is_refused_in_every_spelling() {
        let bad_wait = json!({ "min": "1", "max": "2" });
        for response in [
            json!({ "is": {}, "behaviors": { "wait": bad_wait } }),
            json!({ "is": {}, "behaviors": [{ "repeat": 2 }, { "wait": bad_wait }] }),
            json!({ "statusCode": 200, "_behaviors": { "wait": bad_wait } }),
            json!({ "statusCode": 200, "behaviors": [{ "wait": bad_wait }] }),
        ] {
            let msg = response_error(response.clone());
            assert!(msg.contains("`wait` must be"), "{response}: {msg}");
        }
    }

    // The block is checked before the response type is chosen: rift-lint validates behaviors on
    // every response, so a `proxy`/`inject`/`fault` response is held to the same rule.
    #[test]
    fn an_unparseable_block_is_refused_on_every_response_type() {
        for response in [
            json!({ "proxy": { "to": "http://localhost:1" }, "_behaviors": { "repeat": 2.0 } }),
            json!({ "inject": "function (c) { return {}; }", "_behaviors": { "repeat": 2.0 } }),
            json!({ "fault": "CONNECTION_RESET_BY_PEER", "behaviors": [{ "repeat": 2.0 }] }),
        ] {
            let msg = response_error(response.clone());
            assert!(msg.contains("`repeat`"), "{response}: {msg}");
        }
    }

    // `repeat` is last-wins, so a bad one a later element overrides configures nothing; a step a
    // later `null` removes configures nothing too. rift-lint checks the array the same way. The
    // `{"repeat": 3}` element is refused at a door since #1582 (`Unrecognized behavior`), so this
    // is what a config an older engine stored replays to.
    #[test]
    fn an_overridden_bad_value_in_a_behaviors_array_still_loads() {
        {
            let later = json!({ "wait": null });
            let response = json!({ "is": {}, "behaviors": [
                { "wait": { "min": "1", "max": "2" }, "repeat": 2.5 },
                later,
                { "repeat": 3 }
            ] });
            let door = serde_json::from_value::<StubResponse>(response.clone())
                .expect_err("a door refuses the repeat element")
                .to_string();
            assert!(door.contains("Unrecognized behavior: \"repeat\""), "{door}");
            let resp: StubResponse = crate::imposter::deserialize_replayed(response.clone())
                .unwrap_or_else(|e| panic!("{response}: {e}"));
            let StubResponse::Is {
                behaviors_parsed, ..
            } = resp
            else {
                panic!("expected an Is response");
            };
            let parsed = behaviors_parsed.expect("the merged block parses");
            assert_eq!(parsed.repeat, Some(3));
        }
    }

    // Issue #1198: every step runs, so a bad `wait` a later `wait` follows is live and refused —
    // it is no longer overridden.
    #[test]
    fn a_bad_step_followed_by_a_good_one_is_refused() {
        let msg = response_error(json!({ "is": {}, "behaviors": [
            { "wait": { "min": "1", "max": "2" } },
            { "wait": 50 }
        ] }));
        assert!(msg.contains("`wait`"), "{msg}");
    }

    // A block the engine does not use is not what this refusal is about: `_behaviors` shadows
    // `behaviors`, which is never parsed.
    #[test]
    fn a_shadowed_behaviors_block_is_not_parsed() {
        let resp: StubResponse = serde_json::from_value(json!({
            "is": {},
            "_behaviors": { "wait": 10 },
            "behaviors": { "wait": { "min": "1", "max": "2" } }
        }))
        .expect("the shadowed block is not the one the engine uses");
        let StubResponse::Is {
            behaviors_parsed, ..
        } = resp
        else {
            panic!("expected an Is response");
        };
        assert!(matches!(
            behaviors_parsed.expect("parsed").steps.as_slice(),
            [crate::behaviors::BehaviorStep::Wait(
                crate::behaviors::WaitBehavior::Fixed(10)
            )]
        ));
    }

    /// An all-`null` block is no behaviors at all: it loads, with nothing to run. A `null`
    /// `repeat` is refused (#1582), as Mountebank refuses it.
    #[test]
    fn an_all_null_block_loads_as_no_behaviors() {
        let response = json!({ "is": { "statusCode": 200 }, "_behaviors": {
            "wait": null, "copy": null, "lookup": null,
            "decorate": null, "shellTransform": null
        } });
        let resp: StubResponse = serde_json::from_value(response).expect("loads");
        assert!(matches!(
            resp,
            StubResponse::Is {
                behaviors: None,
                behaviors_parsed: None,
                ..
            }
        ));
    }

    #[test]
    fn every_valid_wait_spelling_and_null_keys_still_load() {
        for block in [
            json!({ "wait": 0 }),
            json!({ "wait": { "min": 1, "max": 9 } }),
            json!({ "wait": "function() { return 5; }" }),
            json!({ "wait": { "inject": "function() { return 5; }" } }),
            json!({ "someFutureKey": 1, "wait": 5 }),
        ] {
            let response = json!({ "is": { "statusCode": 200 }, "_behaviors": block });
            let resp: StubResponse = serde_json::from_value(response.clone())
                .unwrap_or_else(|e| panic!("{response}: {e}"));
            assert!(
                matches!(
                    resp,
                    StubResponse::Is {
                        behaviors_parsed: Some(_),
                        ..
                    }
                ),
                "{response}"
            );
        }
    }

    #[test]
    fn test_stub_recorded_from_roundtrip() {
        let stub_json = json!({
            "predicates": [],
            "responses": [{ "is": {} }],
            "recordedFrom": "http://upstream:8080"
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        assert_eq!(stub.recorded_from.as_deref(), Some("http://upstream:8080"));
        let serialized = serde_json::to_value(&stub).unwrap();
        assert_eq!(serialized["recordedFrom"], json!("http://upstream:8080"));
    }

    // Issue #531: `_rift.fault.tcp` accepts both the bare kind string and the probabilistic object
    // form, and must round-trip in exactly the parsed shape (string↔string, object↔object) — the
    // untagged enum must not normalize one into the other, or the SDK parse-fidelity gate breaks.
    #[test]
    fn rift_tcp_fault_roundtrips_both_forms() {
        let bare = json!("CONNECTION_RESET_BY_PEER");
        let parsed: RiftTcpFault = serde_json::from_value(bare.clone()).unwrap();
        assert!(matches!(parsed, RiftTcpFault::Kind(_)));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), bare);

        let object = json!({ "probability": 0.1, "type": "CONNECTION_RESET_BY_PEER" });
        let parsed: RiftTcpFault = serde_json::from_value(object.clone()).unwrap();
        assert!(matches!(parsed, RiftTcpFault::Probabilistic { .. }));
        assert_eq!(serde_json::to_value(&parsed).unwrap(), object);
    }

    // The object form exists solely to carry `probability`; `{ "type": X }` alone would be a second
    // spelling of the bare form, so it must be rejected rather than silently defaulted.
    #[test]
    fn rift_tcp_fault_object_requires_probability() {
        let missing = json!({ "type": "CONNECTION_RESET_BY_PEER" });
        assert!(serde_json::from_value::<RiftTcpFault>(missing).is_err());
    }

    // The object form must carry both fields: neither `probability` nor `type` alone is a valid
    // object (an object without `type` is not a second spelling of the bare string either).
    #[test]
    fn rift_tcp_fault_object_requires_type() {
        let missing = json!({ "probability": 0.5 });
        assert!(serde_json::from_value::<RiftTcpFault>(missing).is_err());
    }

    // The rejection message must be actionable at the API boundary (not the opaque untagged-enum
    // "data did not match any variant") — it names the offending field.
    #[test]
    fn rift_tcp_fault_errors_are_actionable() {
        let missing_prob = serde_json::from_value::<RiftTcpFault>(json!({ "type": "reset" }))
            .unwrap_err()
            .to_string();
        assert!(
            missing_prob.contains("probability"),
            "message should name 'probability': {missing_prob}"
        );

        let missing_type = serde_json::from_value::<RiftTcpFault>(json!({ "probability": 0.5 }))
            .unwrap_err()
            .to_string();
        assert!(
            missing_type.contains("type"),
            "message should name 'type': {missing_type}"
        );
    }

    // An out-of-range probability is rejected at parse time rather than silently degrading to
    // always/never fires at request time.
    #[test]
    fn rift_tcp_fault_object_rejects_out_of_range_probability() {
        for bad in [
            json!({ "probability": 1.5, "type": "reset" }),
            json!({ "probability": -0.1, "type": "reset" }),
        ] {
            assert!(serde_json::from_value::<RiftTcpFault>(bad).is_err());
        }
        // The boundary values 0.0 and 1.0 are valid.
        assert!(
            serde_json::from_value::<RiftTcpFault>(json!({ "probability": 0.0, "type": "reset" }))
                .is_ok()
        );
        assert!(
            serde_json::from_value::<RiftTcpFault>(json!({ "probability": 1.0, "type": "reset" }))
                .is_ok()
        );
    }

    #[test]
    fn rift_tcp_fault_accessors() {
        let bare: RiftTcpFault = serde_json::from_value(json!("EMPTY_RESPONSE")).unwrap();
        assert_eq!(bare.kind(), "EMPTY_RESPONSE");
        assert_eq!(bare.probability(), 1.0);

        let object: RiftTcpFault =
            serde_json::from_value(json!({ "probability": 0.25, "type": "reset" })).unwrap();
        assert_eq!(object.kind(), "reset");
        assert_eq!(object.probability(), 0.25);
    }

    /// Issue #1610: Mountebank stamps `Date.toJSON()` (milliseconds, `Z`); the width is fixed even
    /// on a whole second, which `to_rfc3339`'s variable-length fraction does not give.
    #[test]
    fn journal_timestamp_is_millisecond_precision_with_z() {
        use chrono::TimeZone;
        let frac = chrono::Utc
            .with_ymd_and_hms(2026, 10, 17, 10, 0, 0)
            .unwrap()
            + chrono::Duration::nanoseconds(123_456_789);
        assert_eq!(journal_timestamp(frac), "2026-10-17T10:00:00.123Z");
        let whole = chrono::Utc
            .with_ymd_and_hms(2026, 10, 17, 10, 0, 0)
            .unwrap();
        assert_eq!(journal_timestamp(whole), "2026-10-17T10:00:00.000Z");
    }

    /// Issue #1610: a journal entry written before the format change (microseconds, `+00:00`)
    /// still decodes and re-serializes its timestamp unchanged.
    #[test]
    fn a_microsecond_offset_timestamp_still_round_trips() {
        let old: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "127.0.0.1:1234", "method": "GET", "path": "/x",
            "query": {}, "headers": {},
            "timestamp": "2026-10-17T10:00:00.123456+00:00",
        }))
        .expect("an old-format timestamp still deserializes");
        assert_eq!(old.timestamp, "2026-10-17T10:00:00.123456+00:00");
        let back = serde_json::to_value(&old).unwrap();
        assert_eq!(back["timestamp"], "2026-10-17T10:00:00.123456+00:00");
    }

    /// Issue #364: recordings persist, so a journal written before these fields existed must still
    /// read back — and must read back as *absent*, not as a zero status served instantly.
    #[test]
    fn a_recording_from_before_the_outcome_fields_still_deserializes() {
        let before: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "127.0.0.1:1234",
            "method": "GET",
            "path": "/x",
            "query": {},
            "headers": {},
            "timestamp": "2026-01-01T00:00:00Z",
        }))
        .expect("a pre-#364 recording still deserializes");

        assert_eq!(before.status, None);
        assert_eq!(before.latency_ms, None);
        assert_eq!(before.node, None);
    }

    /// Issue #364, the other half of the compatibility contract: an entry carrying no outcome
    /// serializes byte-identically to the pre-#364 shape, so nothing that reads existing
    /// recordings sees three new keys appear. Same discipline as `_mode` below.
    #[test]
    fn an_unannotated_recording_omits_the_outcome_fields() {
        let req = RecordedRequest {
            body_omitted: false,
            request_from: "127.0.0.1:1234".to_string(),
            method: "GET".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
            headers: crate::imposter::Headers::new(),
            body: None,
            mode: ResponseMode::Text,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        };
        let value = serde_json::to_value(&req).expect("serializes");
        let object = value.as_object().expect("an object");
        // `latencyMs`, not `latency_ms`: this struct is `rename_all = "camelCase"`, so these are
        // the names a client actually sees — and asserting the snake_case spelling would pass
        // vacuously while proving nothing.
        assert!(!object.contains_key("status"), "absent, not null: {value}");
        assert!(
            !object.contains_key("latencyMs"),
            "absent, not null: {value}"
        );
        assert!(!object.contains_key("node"), "absent, not null: {value}");

        // And when they are set, they are plain fields the console can read.
        let annotated = RecordedRequest {
            body_omitted: false,
            status: Some(503),
            latency_ms: Some(42),
            node: Some("rift-2".to_string()),
            ..req
        };
        let value = serde_json::to_value(&annotated).expect("serializes");
        assert_eq!(value["status"], 503);
        assert_eq!(value["latencyMs"], 42);
        assert_eq!(value["node"], "rift-2");
    }

    // Issue #636: a text-bodied `RecordedRequest` must serialize byte-identically to the
    // pre-#636 shape — no `_mode` key at all — so existing recordings and all-text traffic are
    // unaffected by this change.
    #[test]
    fn recorded_request_text_body_omits_mode_field() {
        let req = RecordedRequest {
            body_omitted: false,
            request_from: "127.0.0.1:1234".to_string(),
            method: "POST".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
            headers: crate::imposter::Headers::new(),
            body: Some("hello".to_string()),
            mode: ResponseMode::Text,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        };
        let value = serde_json::to_value(&req).expect("serializes");
        assert!(
            value.get("_mode").is_none(),
            "a text body must not carry `_mode` on the wire: {value}"
        );
        assert_eq!(value["body"], "hello");
    }

    // Issue #636: a binary body serializes with `_mode: "binary"` and the base64 payload in
    // `body`, mirroring the response side's `IsResponse::mode` (issue #117).
    #[test]
    fn recorded_request_binary_body_carries_mode_field() {
        let req = RecordedRequest {
            body_omitted: false,
            request_from: "127.0.0.1:1234".to_string(),
            method: "POST".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
            headers: crate::imposter::Headers::new(),
            body: Some("//4A".to_string()), // base64 of [0xFF, 0xFE, 0x00]
            mode: ResponseMode::Binary,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        };
        let value = serde_json::to_value(&req).expect("serializes");
        assert_eq!(value["_mode"], "binary");
        assert_eq!(value["body"], "//4A");
    }

    // An entry with no recorded match outcome must serialize byte-identically to the shape that
    // predates `matchOutcome` — no key at all (`ip` is the derived #1602 field). Asserted as exact bytes rather than key-by-key,
    // because the wire compatibility claim is about the whole document, and field order is part
    // of what consumers (and golden fixtures) see.
    #[test]
    fn recorded_request_without_match_outcome_serializes_unchanged() {
        let req = RecordedRequest {
            body_omitted: false,
            request_from: "127.0.0.1:1234".to_string(),
            method: "POST".to_string(),
            path: "/x".to_string(),
            query: Default::default(),
            headers: crate::imposter::Headers::new(),
            body: Some("hello".to_string()),
            mode: ResponseMode::Text,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        };
        assert_eq!(
            serde_json::to_string(&req).expect("serializes"),
            r#"{"requestFrom":"127.0.0.1:1234","method":"POST","path":"/x","query":{},"headers":{},"body":"hello","ip":"127.0.0.1","timestamp":"2026-01-01T00:00:00Z"}"#
        );
    }

    // Issue #1553: a recorded query key sent once is a string and a repeated key an array, as
    // Mountebank prints `requests[].query`; a journal written while Rift comma-joined a repeated key
    // still reads, its joined value as one string, and keys stay case-sensitive.
    #[test]
    fn a_recorded_query_is_a_string_or_an_array_per_key() {
        let recorded = |query: serde_json::Value| -> RecordedRequest {
            serde_json::from_value(json!({
                "requestFrom": "127.0.0.1:1234",
                "method": "GET",
                "path": "/x",
                "query": query,
                "headers": {},
                "timestamp": "2026-01-01T00:00:00Z",
            }))
            .expect("deserializes")
        };

        let new = recorded(json!({"key": ["first", "second"], "one": "1", "Q": "a", "q": "b"}));
        assert_eq!(
            new.query["key"],
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(new.query["one"], vec!["1".to_string()]);
        assert_eq!(
            new.query.len(),
            4,
            "query keys are case-sensitive: {:?}",
            new.query
        );
        let round = serde_json::to_value(&new).expect("serializes");
        assert_eq!(
            round["query"],
            json!({"key": ["first", "second"], "one": "1", "Q": "a", "q": "b"})
        );

        let old = recorded(json!({"key": "first,second"}));
        assert_eq!(old.query["key"], vec!["first,second".to_string()]);
        assert_eq!(
            serde_json::to_value(&old).expect("serializes")["query"],
            json!({"key": "first,second"}),
            "an old journal round-trips byte-identically"
        );
    }

    // Issue #636: a recording written before `_mode` existed has no such key at all — it must
    // still deserialize, defaulting to `Text` (the historical, only-ever behavior).
    #[test]
    fn recorded_request_without_mode_field_deserializes_as_text() {
        let value = json!({
            "requestFrom": "127.0.0.1:1234",
            "method": "GET",
            "path": "/x",
            "query": {},
            "headers": {},
            "body": "hello",
            "timestamp": "2026-01-01T00:00:00Z",
        });
        let req: RecordedRequest = serde_json::from_value(value).expect("deserializes");
        assert_eq!(req.mode, ResponseMode::Text);
    }

    // Issue #1181: a behaviors block on a response no behavior runs on is kept — so GET, `rift save`
    // and `--datadir` return what was written — in both spellings, emitted as Mountebank's array.
    #[test]
    fn a_behaviors_block_on_a_non_is_response_round_trips() {
        let shapes = [
            json!({"proxy": {"to": "http://127.0.0.1:1"}}),
            json!({"inject": "function (config) { return {}; }"}),
            json!({"fault": "CONNECTION_RESET_BY_PEER"}),
            json!({"_rift": {"script": {"engine": "rhai", "code": "1"}}}),
        ];
        for shape in shapes {
            for spelling in [
                json!({"_behaviors": {"wait": 500, "repeat": 2}}),
                json!({"behaviors": [{"wait": 500}], "repeat": 2}),
                // A null `_behaviors` is absent, so `behaviors` is the block read.
                json!({"_behaviors": null, "behaviors": [{"wait": 500}], "repeat": 2}),
            ] {
                let mut written = shape.clone();
                for (k, v) in spelling.as_object().expect("object") {
                    written[k] = v.clone();
                }
                let response: StubResponse =
                    serde_json::from_value(written.clone()).expect("parses");
                let out = serde_json::to_value(StubResponseOut::from(response)).expect("json");
                // Issue #1191: `repeat` is a response field in Mountebank's grammar, not a
                // behavior; an element `{"repeat": 2}` makes Mountebank refuse the file.
                assert_eq!(out["repeat"], json!(2), "{written} -> {out}");
                assert_eq!(
                    out["behaviors"],
                    json!([{"wait": 500}]),
                    "{written} -> {out}"
                );
            }
        }
    }

    // Issue #1181: an empty block is nothing to keep or report, on any shape.
    #[test]
    fn an_empty_behaviors_block_on_a_non_is_response_is_absent() {
        for written in [
            json!({"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": {}}),
            json!({"fault": "CONNECTION_RESET_BY_PEER", "behaviors": []}),
            json!({"inject": "function () {}", "behaviors": [{}]}),
        ] {
            let response: StubResponse = serde_json::from_value(written.clone()).expect("parses");
            let out = serde_json::to_value(StubResponseOut::from(response)).expect("json");
            assert!(out.get("behaviors").is_none(), "{written} -> {out}");
        }
    }

    // Issue #1181: a response carrying only a behaviors block serves its empty 200 *with* the block.
    // A config door refuses it since #1551; a pre-#1551 engine stored it, and it replays this way.
    #[test]
    fn a_behaviors_only_response_keeps_its_block() {
        let response: StubResponse =
            crate::imposter::deserialize_replayed(json!({"_behaviors": {"wait": 500}}))
                .expect("replay admits it");
        let StubResponse::Is {
            behaviors_parsed, ..
        } = &response
        else {
            panic!("expected Is, got {response:?}");
        };
        let steps = &behaviors_parsed.as_ref().expect("parsed").steps;
        assert!(
            matches!(
                steps.as_slice(),
                [crate::behaviors::BehaviorStep::Wait(
                    crate::behaviors::WaitBehavior::Fixed(500)
                )]
            ),
            "{steps:?}"
        );
        let out = serde_json::to_value(StubResponseOut::from(response)).expect("json");
        assert_eq!(out["behaviors"], json!([{"wait": 500}]));
    }

    // Issues #1206, #1209: this enum's size is paid by **every** stub response, and the admin
    // create path materializes each stub several times over (the parsed `config.stubs`, the
    // `Arc<StubState>` clone, and the deep clone the 201 echo serializes), so a byte added here
    // costs several per stub. The 192 inline bytes of `_rift.dataset` measured as +7.7 MB of RSS
    // per 4,000 stubs.
    //
    // The invariant this pins: **every `RiftResponseExtension` reachable from a `StubResponse` is
    // behind a `Box`**. With all five of them boxed the ceiling is `Proxy`'s inline
    // `ProxyResponse`, so a new `_rift` feature costs the extension's own size and nothing per
    // response. Embedding one by value again — on any variant — fails here instead of shipping.
    #[test]
    fn stub_response_stays_small() {
        let size = std::mem::size_of::<StubResponse>();
        assert!(
            size <= 248,
            "StubResponse is {size} bytes, over its budget: box the payload rather than embedding \
             it by value, or — if the growth is deliberate — raise this bound with the RSS \
             measurement that justifies it (issues #1206, #1209)"
        );
    }

    // Issue #1209: `Is.rift` and `RiftScript.rift` moved behind a `Box`, and both are read back
    // out at the serde shim. A field dropped there would not fail loudly — it would erase a
    // `_rift` block the user wrote from the admin echo *and* from what `--datadir` persists, the
    // silent-erasure defect class of #1152/#1181. So the round trip is the contract for both.
    #[test]
    fn a_rift_block_on_is_and_a_script_only_response_survives_the_round_trip() {
        // Carries a `dataset`, so the extension's box and the binding's box (#1206) are both on
        // the wire at once — the nesting no other test on these two variants exercises.
        let rift = json!({
            "templated": true,
            "dataset": {
                "name": "customers",
                "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                "keyColumn": "customer_id",
                "into": "${row}"
            }
        });
        let script_rift = json!({
            "templated": false,
            "script": { "engine": "js", "code": "response.statusCode = 201;" }
        });

        for (written, shape) in [
            (json!({ "is": { "statusCode": 200 }, "_rift": rift }), "is"),
            (json!({ "_rift": script_rift }), "riftScript"),
        ] {
            let parsed: StubResponse = serde_json::from_value(written.clone()).expect("parses");
            // The response must reach the variant its own keys name — a `_rift` block that
            // round-tripped off the wrong variant would still pass the comparison below.
            let (reached, carried) = match &parsed {
                StubResponse::Is { rift, .. } => ("is", rift.is_some()),
                StubResponse::RiftScript { .. } => ("riftScript", true),
                StubResponse::Proxy { .. } => ("proxy", false),
                StubResponse::Inject { .. } => ("inject", false),
                StubResponse::Fault { .. } => ("fault", false),
            };
            assert_eq!(
                reached, shape,
                "{written}: parsed as the wrong response shape"
            );
            assert!(carried, "{written}: the _rift block was dropped on parse");

            let read_back = serde_json::to_value(&parsed).expect("serializes");
            assert_eq!(
                read_back["_rift"], written["_rift"],
                "{written}: _rift did not round-trip"
            );
        }
    }

    // Issue #1206: `ignored_rift` now lives behind a `Box`. The block is reported and round-tripped,
    // never run, so the round trip is the whole contract — a `_rift` block that vanished on parse
    // would take `config_key_ignored` (#1177) with it and read back as if it had never been written.
    #[test]
    fn a_rift_block_on_proxy_inject_and_fault_survives_the_round_trip() {
        // The block carries a `dataset`, so both boxes — the extension and the binding inside it
        // — are on the wire at once, which no other test exercises.
        let rift = json!({
            "templated": true,
            "dataset": {
                "name": "customers",
                "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                "keyColumn": "customer_id",
                "into": "${row}"
            }
        });
        for (written, shape) in [
            (
                json!({ "proxy": { "to": "http://upstream" }, "_rift": rift }),
                "proxy",
            ),
            (
                json!({ "inject": "function () { return {}; }", "_rift": rift }),
                "inject",
            ),
            (
                json!({ "fault": "CONNECTION_RESET_BY_PEER", "_rift": rift }),
                "fault",
            ),
        ] {
            let parsed: StubResponse = serde_json::from_value(written.clone()).expect("parses");
            // The response must reach the variant its own key names — a `_rift` block that
            // round-trips off the wrong variant would still pass the comparison below.
            let (reached, carried) = match &parsed {
                StubResponse::Proxy { ignored_rift, .. } => ("proxy", ignored_rift.is_some()),
                StubResponse::Inject { ignored_rift, .. } => ("inject", ignored_rift.is_some()),
                StubResponse::Fault { ignored_rift, .. } => ("fault", ignored_rift.is_some()),
                StubResponse::Is { .. } => ("is", false),
                StubResponse::RiftScript { .. } => ("riftScript", false),
            };
            assert_eq!(
                reached, shape,
                "{written}: parsed as the wrong response shape"
            );
            assert!(carried, "{written}: the _rift block was dropped on parse");
            let read_back = serde_json::to_value(&parsed).expect("serializes");
            assert_eq!(
                read_back["_rift"], rift,
                "{written}: _rift did not round-trip"
            );
        }
    }

    // Issue #1206: `_rift.dataset` moved behind a `Box` inside `RiftResponseExtension`. It is
    // carried for `rift-cluster` and never executed here, so the only thing that can break is the
    // round trip — and a binding that silently vanished could not be stored at all (#973).
    #[test]
    fn a_rift_dataset_block_survives_the_round_trip_on_a_response() {
        let rift = json!({
            "dataset": {
                "name": "customers",
                "key": { "from": { "query": "id" }, "using": { "method": "regex", "selector": ".*" } },
                "keyColumn": "customer_id",
                "into": "${row}"
            }
        });
        let written = json!({ "is": { "statusCode": 200 }, "_rift": rift });

        let parsed: StubResponse = serde_json::from_value(written.clone()).expect("parses");
        let StubResponse::Is {
            rift: Some(ext), ..
        } = &parsed
        else {
            panic!("a `_rift` block beside an `is` response must reach the Is variant");
        };
        let binding = ext
            .dataset
            .as_ref()
            .expect("the dataset binding was dropped on parse");
        assert_eq!(binding.name, "customers");
        assert_eq!(binding.key_column, "customer_id");
        assert_eq!(binding.version, None);

        // `templated: false` is written unconditionally by the extension's own serde shape, so the
        // binding — the field this change moved — is what is compared.
        let read_back = serde_json::to_value(&parsed).expect("serializes");
        assert_eq!(read_back["_rift"]["dataset"], rift["dataset"]);
    }
}

/// Issue #1191: what `GET /imposters`, `rift save` and `--datadir` write must load in Mountebank.
/// Each expectation below is Mountebank 2.9.1's own grammar, probed against a live `mb`.
#[cfg(test)]
mod mountebank_output_tests {
    use super::*;
    use serde_json::{Value, json};

    fn out(response: Value) -> Value {
        let parsed: StubResponse = serde_json::from_value(response.clone())
            .unwrap_or_else(|e| panic!("{response} does not parse: {e}"));
        serde_json::to_value(StubResponseOut::from(parsed)).expect("json")
    }

    /// [`out`] for a shape a config door refuses since #1582 but an older engine admitted and an
    /// embedder replays: what it is written back as.
    fn replayed_out(response: Value) -> Value {
        assert!(
            serde_json::from_value::<StubResponse>(response.clone()).is_err(),
            "{response} is refused at a config door"
        );
        let parsed: StubResponse = crate::imposter::deserialize_replayed(response.clone())
            .unwrap_or_else(|e| panic!("{response} does not replay: {e}"));
        serde_json::to_value(StubResponseOut::from(parsed)).expect("json")
    }

    #[test]
    fn repeat_is_a_top_level_field_on_every_response_shape() {
        for shape in [
            json!({"is": {"body": "a"}}),
            json!({"proxy": {"to": "http://127.0.0.1:1"}}),
            json!({"inject": "function (config) { return {}; }"}),
            json!({"fault": "CONNECTION_RESET_BY_PEER"}),
            json!({"_rift": {"script": {"engine": "rhai", "code": "1"}}}),
        ] {
            for (spelling, value) in [
                ("_behaviors", json!({"repeat": 3, "wait": 500})),
                // Rift's pre-0.18.0 spelling, refused at a door since #1582.
                ("behaviors", json!([{"wait": 500}, {"repeat": 3}])),
            ] {
                let mut written = shape.clone();
                written[spelling] = value;
                let saved = if spelling == "behaviors" {
                    replayed_out(written.clone())
                } else {
                    out(written.clone())
                };
                assert_eq!(saved["repeat"], json!(3), "{written} -> {saved}");
                assert_eq!(
                    saved["behaviors"],
                    json!([{"wait": 500}]),
                    "{written} -> {saved}"
                );
            }
        }
    }

    #[test]
    fn a_repeat_only_block_writes_no_behaviors_array() {
        let saved = out(json!({"is": {"body": "a"}, "_behaviors": {"repeat": 2}}));
        assert_eq!(saved["repeat"], json!(2));
        assert!(saved.get("behaviors").is_none(), "{saved}");
        let saved = out(json!({"is": {"body": "a"}, "repeat": 2}));
        assert_eq!(saved["repeat"], json!(2));
        assert!(saved.get("behaviors").is_none(), "{saved}");
    }

    /// Mountebank and a config door (#1582) refuse a `0` or `null` `repeat`; one an older engine
    /// admitted replays, and Rift serves `0` as `1`, which is what an absent `repeat` is.
    #[test]
    fn a_zero_or_null_repeat_is_not_written() {
        for block in [
            json!({"repeat": 0, "wait": 1}),
            json!({"repeat": null, "wait": 1}),
        ] {
            let saved = replayed_out(json!({"is": {"body": "a"}, "_behaviors": block}));
            assert!(saved.get("repeat").is_none(), "{block} -> {saved}");
            assert_eq!(
                saved["behaviors"],
                json!([{"wait": 1}]),
                "{block} -> {saved}"
            );
        }
    }

    /// In Mountebank's array form, element order is execution order; the object form's is
    /// Mountebank's upcast order (issue #1198).
    #[test]
    fn behaviors_are_written_in_execution_order() {
        let saved = out(json!({"is": {"body": "a"}, "_behaviors": {
            "shellTransform": "cat",
            "decorate": "function (req, res) {}",
            "lookup": {"key": {"from": "path", "using": {"method": "regex", "selector": ".+"}},
                       "fromDataSource": {"csv": {"path": "x.csv", "keyColumn": "k"}},
                       "into": "${row}"},
            "copy": {"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".+"}},
            "wait": 5
        }}));
        let keys: Vec<String> = saved["behaviors"]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| {
                e.as_object()
                    .expect("object")
                    .keys()
                    .next()
                    .expect("key")
                    .clone()
            })
            .collect();
        assert_eq!(
            keys,
            ["wait", "lookup", "copy", "shellTransform", "decorate"]
        );
    }

    #[test]
    fn a_list_is_written_one_element_per_item() {
        let copy_a =
            json!({"from": "path", "into": "${A}", "using": {"method": "regex", "selector": ".+"}});
        let copy_b =
            json!({"from": "path", "into": "${B}", "using": {"method": "regex", "selector": ".+"}});
        let lookup = json!({"key": {"from": "path", "using": {"method": "regex", "selector": ".+"}},
                            "fromDataSource": {"csv": {"path": "x.csv", "keyColumn": "k"}},
                            "into": "${row}"});
        let saved = out(json!({"is": {"body": "a"}, "_behaviors": {
            "copy": [copy_a.clone()],
            "lookup": [lookup.clone()],
            "shellTransform": ["echo x"]
        }}));
        assert_eq!(
            saved["behaviors"],
            json!([{"lookup": lookup}, {"copy": copy_a.clone()}, {"shellTransform": "echo x"}])
        );
        // Mountebank spells several items as one element each, and so does Rift (#1199); a list
        // inside one element is refused by Mountebank (`copy behavior "from" field required`).
        let saved = out(json!({"is": {"body": "a"}, "_behaviors": {
            "copy": [copy_a.clone(), copy_b.clone()],
            "shellTransform": ["echo x", "echo y"]
        }}));
        assert_eq!(
            saved["behaviors"],
            json!([
                {"copy": copy_a}, {"copy": copy_b},
                {"shellTransform": "echo x"}, {"shellTransform": "echo y"}
            ])
        );
    }

    /// Issue #1188/#1198: a top-level `repeat` wins over one in the block, whichever form the
    /// block takes, and is what the cycler reads.
    #[test]
    fn a_top_level_repeat_wins_over_the_blocks() {
        use crate::behaviors::HasRepeatBehavior;
        for response in [
            json!({"is": {}, "repeat": 4, "_behaviors": {"repeat": 2, "wait": 1}}),
            // Refused at a door since #1582; an older engine's stored config replays.
            json!({"is": {}, "repeat": 4, "behaviors": [{"repeat": 2}, {"wait": 1}]}),
        ] {
            let parsed: StubResponse =
                crate::imposter::deserialize_replayed(response.clone()).expect("parses");
            assert_eq!(parsed.get_repeat(), Some(4), "{response}");
            assert_eq!(
                parsed.behaviors_block(),
                Some(&json!([{"repeat": 4}, {"wait": 1}])),
                "{response}"
            );
        }
    }

    /// Issue #1198: compiling a compiled program changes nothing, so every constructor can
    /// compile again without reshaping what the parser, the gate and the writer read.
    #[test]
    fn compiling_a_program_twice_changes_nothing() {
        let copy = |t: &str| json!({"from": "path", "into": t, "using": {"method": "regex", "selector": ".+"}});
        for block in [
            json!({"repeat": 2, "wait": 1, "copy": [copy("${A}"), copy("${B}")], "decorate": "f",
                   "shellTransform": ["x", "y"], "someFutureKey": 1}),
            json!([{"decorate": "f"}, {"repeat": 3}, {"wait": 1}, {"decorate": "g"}, {"repeat": null}]),
            json!([{"copy": copy("${A}")}, {"copy": null}, {"copy": [copy("${B}")]}]),
            // Not a list of items: left whole, for the parser to refuse, and stable.
            json!({"copy": [[copy("${A}")]]}),
            json!({"shellTransform": ["x", null]}),
        ] {
            let once = compile_behaviors(block.clone());
            let twice = once.clone().and_then(compile_behaviors);
            assert_eq!(once, twice, "{block}");
        }
    }

    /// A list item that is itself a list, or `null`, is not an item: the parser refuses the block,
    /// as it did before #1198, instead of it being flattened or silently dropped.
    #[test]
    fn a_nested_or_null_list_item_is_still_refused() {
        let copy =
            json!({"from": "path", "into": "${A}", "using": {"method": "regex", "selector": ".+"}});
        for block in [
            json!({"copy": [[copy.clone()]]}),
            json!({"copy": [copy.clone(), null]}),
            json!({"shellTransform": [["a", "b"]]}),
            json!({"shellTransform": ["echo x", null]}),
        ] {
            let response = json!({"is": {}, "_behaviors": block});
            assert!(
                serde_json::from_value::<StubResponse>(response.clone()).is_err(),
                "{response} must be refused"
            );
        }
    }

    /// Issue #1195: every repeated list element is written back — and, since #1198, in the order it
    /// runs, so elements that are not adjacent are not merged into one list.
    #[test]
    fn repeated_list_elements_are_all_written_back() {
        let copy_a =
            json!({"from": "path", "into": "${A}", "using": {"method": "regex", "selector": ".+"}});
        let copy_b = json!({"from": "method", "into": "${B}", "using": {"method": "regex", "selector": ".+"}});
        let saved = out(json!({"is": {"body": "a"}, "behaviors": [
            {"copy": copy_a.clone()}, {"shellTransform": "echo x"},
            {"copy": copy_b.clone()}, {"shellTransform": "echo y"}
        ]}));
        assert_eq!(
            saved["behaviors"],
            json!([
                {"copy": copy_a}, {"shellTransform": "echo x"},
                {"copy": copy_b}, {"shellTransform": "echo y"}
            ])
        );
    }

    #[test]
    fn empty_and_null_behaviors_are_not_written() {
        let saved = out(json!({"is": {"body": "a"}, "_behaviors": {
            "copy": [], "lookup": [], "shellTransform": [], "decorate": null, "wait": 7
        }}));
        assert_eq!(saved["behaviors"], json!([{"wait": 7}]));
    }

    /// The configuration a response's block parses to, and the `repeat` the cycler reads.
    fn configured(response: &Value) -> (Value, Option<u32>) {
        use crate::behaviors::HasRepeatBehavior;
        let parsed: StubResponse = serde_json::from_value(response.clone()).expect("parses");
        // The compiled program is the configuration: compare it directly.
        let block = parsed.behaviors_block().cloned().unwrap_or(Value::Null);
        (block, parsed.get_repeat())
    }

    /// What is written must read back as the same configuration — compared as parsed behaviors,
    /// because every list is written one element per item.
    #[test]
    fn the_written_form_reads_back_to_the_same_behaviors() {
        for written in [
            json!({"is": {"body": "a"}, "_behaviors": {"repeat": 2, "wait": {"min": 1, "max": 5}}}),
            json!({"is": {"body": "a"}, "_behaviors": {
                "copy": [{"from": "path", "into": "${P}", "using": {"method": "regex", "selector": ".+"}}],
                "shellTransform": ["echo x"], "decorate": "function (req, res) {}"
            }}),
            json!({"proxy": {"to": "http://127.0.0.1:1"}, "_behaviors": {"repeat": 4}}),
            // Issue #1199: a list in the object form is written one element per item.
            json!({"is": {"body": "a"}, "_behaviors": {"copy": [
                {"from": "path", "into": "${A}", "using": {"method": "regex", "selector": ".+"}},
                {"from": "method", "into": "${B}", "using": {"method": "regex", "selector": ".+"}}
            ], "shellTransform": ["echo x", "echo y"]}}),
            // Issue #1195: Mountebank's one-element-per-item spelling.
            json!({"is": {"body": "a"}, "behaviors": [
                {"copy": {"from": "path", "into": "${A}", "using": {"method": "regex", "selector": ".+"}}},
                {"copy": {"from": "method", "into": "${B}", "using": {"method": "regex", "selector": ".+"}}},
                {"shellTransform": "echo x"}, {"shellTransform": "echo y"}
            ]}),
        ] {
            let saved = out(written.clone());
            assert_eq!(
                configured(&saved),
                configured(&written),
                "{written} -> {saved}"
            );
            assert_eq!(
                out(saved.clone()),
                saved,
                "{written}: writing is not stable"
            );
        }
    }

    // Issue #1308: `Gate::covers` truth table.
    #[test]
    fn gate_covers_truth_table() {
        let stub = |space: Option<&str>, scenario: Option<(&str, &str)>| -> Stub {
            let mut v = json!({ "predicates": [], "responses": [{ "is": {} }] });
            if let Some(space) = space {
                v["space"] = json!(space);
            }
            if let Some((name, state)) = scenario {
                v["scenarioName"] = json!(name);
                v["requiredScenarioState"] = json!(state);
            }
            serde_json::from_value(v).expect("stub")
        };
        let open = stub(None, None);
        let a = stub(Some("a"), None);
        let b = stub(Some("b"), None);
        let started = stub(None, Some(("s", "Started")));
        let paid = stub(None, Some(("s", "paid")));
        let other_scenario = stub(None, Some(("t", "Started")));
        let a_started = stub(Some("a"), Some(("s", "Started")));

        // none/none
        assert!(open.gate().covers(&open.gate()));
        // none covers some, some does not cover none
        assert!(open.gate().covers(&a.gate()));
        assert!(open.gate().covers(&started.gate()));
        assert!(open.gate().covers(&a_started.gate()));
        assert!(!a.gate().covers(&open.gate()));
        assert!(!started.gate().covers(&open.gate()));
        // same/same
        assert!(a.gate().covers(&a.gate()));
        assert!(started.gate().covers(&started.gate()));
        // different
        assert!(!a.gate().covers(&b.gate()));
        assert!(!started.gate().covers(&paid.gate()));
        assert!(!started.gate().covers(&other_scenario.gate()));
        // cross-space/scenario: each axis is checked independently
        assert!(a.gate().covers(&a_started.gate()));
        assert!(started.gate().covers(&a_started.gate()));
        assert!(!a_started.gate().covers(&a.gate()));
        assert!(!a.gate().covers(&started.gate()));
        // a scenario name alone gates nothing
        let name_only: Stub = serde_json::from_value(
            json!({ "scenarioName": "s", "predicates": [], "responses": [{ "is": {} }] }),
        )
        .expect("stub");
        assert_eq!(name_only.gate(), open.gate());
    }
}

/// Issue #1361: the printed imposter is a normalized form. `is.statusCode` prints as a number,
/// like `defaultResponse.statusCode` and Mountebank's own recordings, and fields left at their
/// empty or default value are not printed. Decoding is unchanged, so nothing is lost.
#[cfg(test)]
mod normalized_print_tests {
    use super::*;
    use serde_json::{Value, json};

    fn printed_stub(stub: Value) -> Value {
        let parsed: Stub = serde_json::from_value(stub).expect("stub");
        serde_json::to_value(&parsed).expect("json")
    }

    #[test]
    fn is_status_code_prints_as_number() {
        for written in [json!("202"), json!(202)] {
            let printed = printed_stub(json!({"responses": [{"is": {"statusCode": written}}]}));
            assert_eq!(
                printed["responses"][0]["is"]["statusCode"],
                json!(202),
                "{printed}"
            );
        }
    }

    #[test]
    fn default_fields_are_not_printed() {
        let printed = printed_stub(json!({"responses": [{"proxy": {"to": "http://x"}}]}));
        assert!(printed.get("predicates").is_none(), "{printed}");
        let proxy = &printed["responses"][0]["proxy"];
        assert_eq!(proxy, &json!({"to": "http://x"}));

        let printed = printed_stub(json!({
            "predicates": [{"equals": {"path": "/p"}}],
            "responses": [{"proxy": {
                "to": "http://x",
                "mode": "proxyAlways",
                "addWaitBehavior": true,
                "predicateGenerators": [{"matches": {"path": true}}],
                "injectHeaders": {"X-Via": "rift"}
            }}]
        }));
        assert_eq!(printed["predicates"], json!([{"equals": {"path": "/p"}}]));
        let proxy = &printed["responses"][0]["proxy"];
        assert_eq!(proxy["mode"], json!("proxyAlways"), "{proxy}");
        assert_eq!(proxy["addWaitBehavior"], json!(true), "{proxy}");
        assert_eq!(
            proxy["predicateGenerators"],
            json!([{"matches": {"path": true}}]),
            "{proxy}"
        );
        assert_eq!(proxy["injectHeaders"], json!({"X-Via": "rift"}), "{proxy}");
    }

    #[test]
    fn default_response_empty_headers_not_printed() {
        for default_response in [
            json!({"statusCode": 404}),
            json!({"statusCode": 404, "headers": {}}),
        ] {
            let config: ImposterConfig = serde_json::from_value(json!({
                "protocol": "http",
                "defaultResponse": default_response
            }))
            .expect("config");
            let printed = serde_json::to_value(&config).expect("json");
            assert_eq!(
                printed["defaultResponse"],
                json!({"statusCode": 404}),
                "{printed}"
            );
        }
    }

    /// A stub's written `headers: {}` replaces the default's headers with none (#1369), so unlike
    /// `defaultResponse.headers` it is printed, and survives the print → re-load round trip that
    /// `--datadir` and `rift save` make.
    #[test]
    fn stub_is_empty_headers_survive_a_round_trip() {
        let config: ImposterConfig = serde_json::from_value(json!({
            "protocol": "http",
            "defaultResponse": {"statusCode": 418, "headers": {"X-Default": "yes"}},
            "stubs": [{"responses": [{"is": {"headers": {}}}]}]
        }))
        .expect("config");
        let printed = serde_json::to_value(&config).expect("json");
        assert_eq!(
            printed["stubs"][0]["responses"][0]["is"],
            json!({"headers": {}})
        );

        let reloaded: ImposterConfig = serde_json::from_value(printed).expect("re-load");
        let default = reloaded.default_response.as_ref().expect("defaultResponse");
        let Some(StubResponse::Is { is, .. }) = reloaded.stubs[0].responses.first() else {
            panic!("an is response");
        };
        let served = is.completed_by(default);
        assert!(served.headers.is_empty(), "{:?}", served.headers);
        assert_eq!(served.status_code, 418);
    }
}

#[cfg(test)]
mod caller_address_error_tests {
    use super::{ImposterError, UnresolvableHost};
    use std::io::{Error, ErrorKind};

    fn bind(kind: ErrorKind) -> ImposterError {
        ImposterError::BindError(8080, anyhow::Error::new(Error::from(kind)))
    }

    #[test]
    fn address_kinds_are_the_callers_mistake() {
        assert!(bind(ErrorKind::AddrInUse).is_caller_address_error());
        assert!(bind(ErrorKind::AddrNotAvailable).is_caller_address_error());
        assert!(bind(ErrorKind::PermissionDenied).is_caller_address_error());
    }

    #[test]
    fn a_context_wrapped_address_error_is_still_classified() {
        let wrapped = anyhow::Error::new(Error::from(ErrorKind::AddrInUse)).context("bind");
        assert!(ImposterError::BindError(8080, wrapped).is_caller_address_error());
    }

    #[test]
    fn an_unresolvable_host_is_the_callers_mistake() {
        let source = anyhow::Error::new(Error::other("lookup failed")).context(UnresolvableHost);
        assert!(ImposterError::BindError(8080, source).is_caller_address_error());
    }

    #[test]
    fn engine_faults_and_other_variants_are_not() {
        assert!(!bind(ErrorKind::Other).is_caller_address_error());
        assert!(!bind(ErrorKind::OutOfMemory).is_caller_address_error());
        assert!(
            !ImposterError::BindError(8080, anyhow::anyhow!("no socket address"))
                .is_caller_address_error()
        );
        assert!(!ImposterError::PortInUse(8080).is_caller_address_error());
    }
}

#[cfg(test)]
mod recorded_query_order_tests {
    use super::RecordedRequest;
    use serde_json::json;

    fn recorded(query: &str) -> RecordedRequest {
        let mut req: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "127.0.0.1:1", "method": "GET", "path": "/",
            "query": {}, "headers": {}, "timestamp": "t"
        }))
        .expect("recorded request");
        req.query = crate::util::parse_query_string(query).into_iter().collect();
        req
    }

    fn query_text(req: &RecordedRequest) -> String {
        let text = serde_json::to_string(req).expect("serializes");
        let start = text.find("\"query\":").expect("query") + "\"query\":".len();
        let end = text[start..].find("},").expect("end") + start + 1;
        text[start..end].to_string()
    }

    // Issue #1596: the journal lists a query in request order, repeated key at its first position.
    #[test]
    fn the_journal_keeps_request_order() {
        assert_eq!(
            query_text(&recorded("b=1&a=2&c=3&a=5")),
            r#"{"b":"1","a":["2","5"],"c":"3"}"#
        );
    }

    // Issue #1596: array-index keys first, ascending, as Mountebank's `JSON.stringify` prints them.
    #[test]
    fn the_journal_lists_integer_keys_first_as_a_js_object_does() {
        assert_eq!(
            query_text(&recorded("b=1&10=x&2=y&a=3")),
            r#"{"2":"y","10":"x","b":"1","a":"3"}"#
        );
    }

    // The wire shape is unchanged, so a journal written in any key order still decodes.
    #[test]
    fn a_journal_in_any_key_order_decodes() {
        let req: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "x", "method": "GET", "path": "/", "headers": {}, "timestamp": "t",
            "query": {"z": "1", "a": ["2", "3"]}
        }))
        .expect("decodes");
        assert_eq!(req.query["a"], ["2", "3"]);
        assert_eq!(req.query["z"], ["1"]);
    }
}

/// Issue #1602: `ip` and `form` are derived when a recorded request is printed, as Mountebank's
/// `httpRequest.js` records them, and never stored.
#[cfg(test)]
mod recorded_request_ip_form_tests {
    use super::*;
    use serde_json::{Value, json};

    fn printed(from: &str, content_type: Option<&str>, body: Option<&str>) -> Value {
        let mut doc = json!({
            "requestFrom": from, "method": "POST", "path": "/", "query": {}, "headers": {},
            "timestamp": "2024-01-15T10:30:00.000Z"
        });
        if let Some(ct) = content_type {
            doc["headers"] = json!({ "Content-Type": ct });
        }
        if let Some(b) = body {
            doc["body"] = json!(b);
        }
        let req: RecordedRequest = serde_json::from_value(doc).expect("valid recording");
        serde_json::to_value(&req).expect("serializes")
    }

    const FORM: &str = "application/x-www-form-urlencoded";

    #[test]
    fn ip_is_the_client_address_without_its_port() {
        assert_eq!(printed("[::1]:5000", None, None)["ip"], json!("::1"));
        assert_eq!(
            printed("127.0.0.1:5000", None, None)["ip"],
            json!("127.0.0.1")
        );
        // Tests and embedders record a bare IP.
        assert_eq!(printed("10.0.0.7", None, None)["ip"], json!("10.0.0.7"));
    }

    #[test]
    fn an_unparseable_request_from_omits_ip() {
        assert!(printed("not-an-address", None, None).get("ip").is_none());
    }

    #[test]
    fn form_is_printed_for_a_urlencoded_body_only() {
        let form = printed("[::1]:1", Some(FORM), Some("a=1&a=2&b=x%20y&c"))["form"].clone();
        assert_eq!(form, json!({"a": ["1", "2"], "b": "x y", "c": ""}));

        assert!(
            printed("[::1]:1", Some("application/json"), Some("a=1"))
                .get("form")
                .is_none()
        );
        assert!(
            printed("[::1]:1", Some(FORM), Some(""))
                .get("form")
                .is_none()
        );
        assert!(printed("[::1]:1", Some(FORM), None).get("form").is_none());
    }

    // `form` prints in the client's order, array-index keys first, as `query` does (issue #1596).
    #[test]
    fn form_keeps_the_clients_key_order() {
        let req: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "[::1]:1", "method": "POST", "path": "/", "query": {},
            "headers": {"Content-Type": FORM}, "body": "z=1&a=2&5=x&z=3", "timestamp": "t"
        }))
        .expect("valid");
        let text = serde_json::to_string(&req).expect("json");
        assert!(
            text.contains(r#""form":{"5":"x","z":["1","3"],"a":"2"}"#),
            "{text}"
        );
    }

    #[test]
    fn form_is_never_printed_for_a_binary_or_omitted_body() {
        let mut req: RecordedRequest = serde_json::from_value(json!({
            "requestFrom": "[::1]:1", "method": "POST", "path": "/", "query": {},
            "headers": {"Content-Type": FORM}, "body": "YT0x", "_mode": "binary",
            "timestamp": "t"
        }))
        .expect("valid");
        assert!(
            serde_json::to_value(&req)
                .expect("json")
                .get("form")
                .is_none()
        );
        req.mode = ResponseMode::Text;
        req.body = None;
        req.body_omitted = true;
        assert!(
            serde_json::to_value(&req)
                .expect("json")
                .get("form")
                .is_none()
        );
    }

    #[test]
    fn an_incoming_ip_or_form_is_ignored_and_the_output_is_stable() {
        let first = printed("[::1]:1", Some(FORM), Some("a=1"));
        let mut again = first.clone();
        again["ip"] = json!("9.9.9.9");
        again["form"] = json!({"zzz": "1"});
        let req: RecordedRequest = serde_json::from_value(again).expect("decodes");
        assert_eq!(serde_json::to_value(&req).expect("json"), first);
    }
}
