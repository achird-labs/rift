//! Type definitions for Mountebank-compatible imposter management.
//!
//! This module contains all the structs, enums, and type aliases used by the imposter system.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Serde for multi-value headers (issue #238). Accepts the Mountebank-style `"k": "v"` *and*
/// `"k": ["v1", "v2"]` on the wire; serializes a single value back as a plain string and multiple
/// values as an array, so existing single-value consumers are unaffected.
pub(crate) mod multi_value_headers {
    use serde::Deserialize;
    use serde::de::Deserializer;
    use serde::ser::{SerializeMap, Serializer};
    use std::collections::HashMap;

    pub fn serialize<S: Serializer>(
        headers: &HashMap<String, Vec<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(headers.len()))?;
        for (key, values) in headers {
            match values.as_slice() {
                [] => continue, // a key with no values would emit no header line; omit it
                [single] => map.serialize_entry(key, single)?,
                many => map.serialize_entry(key, many)?,
            }
        }
        map.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HashMap<String, Vec<String>>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(String),
            Many(Vec<String>),
        }
        let raw = HashMap::<String, OneOrMany>::deserialize(deserializer)?;
        Ok(raw
            .into_iter()
            .map(|(k, v)| match v {
                OneOrMany::One(s) => (k, vec![s]),
                OneOrMany::Many(v) => (k, v),
            })
            .collect())
    }
}

// ============================================================================
// Recorded Request Types
// ============================================================================

/// Recorded request for imposter
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedRequest {
    pub request_from: String,
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    #[serde(default, with = "multi_value_headers")]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub timestamp: String,
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
    pub headers: HashMap<String, String>,
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
    pub headers: Option<HashMap<String, String>>,
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
/// Field ordering matches Mountebank output: scenarioName, predicates, responses, _links
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "StubRaw")]
pub struct Stub {
    /// Optional scenario name for documentation/organization (Mountebank compatible)
    /// Placed first to match Mountebank output ordering
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
    #[serde(default)]
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

/// Raw deserialization type for Stub — handles alternative field names and format conversions:
/// - `rules` as an alias for `predicates`
/// - `delayRange` array (stub-level latency) converted to per-response `wait` behavior
/// - `recordedFrom` URL from Mountebank proxy recording
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StubRaw {
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
struct DelayRange {
    #[serde(deserialize_with = "de_u64_or_string")]
    min: u64,
    #[serde(deserialize_with = "de_u64_or_string")]
    max: u64,
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

impl From<StubRaw> for Stub {
    fn from(raw: StubRaw) -> Self {
        // "rules" is an alias for "predicates"; prefer "predicates" when both present
        let predicates = if !raw.predicates.is_empty() {
            raw.predicates
        } else {
            raw.rules
        };

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

        Stub {
            scenario_name: raw.scenario_name,
            required_scenario_state: raw.required_scenario_state,
            new_scenario_state: raw.new_scenario_state,
            space: raw.space,
            id: raw.id,
            predicates,
            responses,
            recorded_from: raw.recorded_from,
            verify: raw.verify,
        }
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
        } => {
            let behaviors = Some(match behaviors {
                Some(serde_json::Value::Object(mut obj)) => {
                    obj.entry("wait").or_insert(wait_val);
                    serde_json::Value::Object(obj)
                }
                Some(other) => other,
                None => serde_json::json!({ "wait": wait_val }),
            });
            StubResponse::Is {
                is,
                behaviors,
                rift,
            }
        }
        other => other,
    }
}

// Predicate types live in the shared `rift-types` crate (issue #36) so the proxy and the
// linter share one definition. Re-exported here so existing `crate::imposter::types::*`
// paths keep resolving unchanged.
pub use rift_types::{Predicate, PredicateOperation, PredicateParameters, PredicateSelector};

/// Response within a stub - wrapper type that handles various formats
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "StubResponseRaw", into = "StubResponseOut")]
pub enum StubResponse {
    Is {
        is: IsResponse,
        #[serde(rename = "_behaviors", skip_serializing_if = "Option::is_none")]
        behaviors: Option<serde_json::Value>,
        #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
        rift: Option<RiftResponseExtension>,
    },
    Proxy {
        proxy: ProxyResponse,
    },
    Inject {
        inject: String,
    },
    Fault {
        fault: String,
    },
    /// Rift script-only response (no `is` block, response generated by script)
    RiftScript {
        rift: RiftResponseExtension,
    },
}

/// Raw deserialization type that handles multiple JSON formats for stub responses
/// Supports:
/// - Standard Mountebank format with `is`, `proxy`, `inject`, or `fault` fields
/// - Formats with `behaviors` (without underscore) or `_behaviors`
/// - Formats with `proxy: null` alongside `is` (ignored)
/// - `statusCode` as either string or number
/// - Rift extensions via `_rift` field
#[derive(Debug, Clone, Deserialize)]
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
    /// Mountebank-style behaviors (with underscore prefix) - for deserialization
    #[serde(rename = "_behaviors", skip_serializing_if = "Option::is_none")]
    pub underscore_behaviors: Option<serde_json::Value>,
    /// Alternative behaviors field (without underscore, used by some tools) - for deserialization
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behaviors: Option<serde_json::Value>,
    /// Rift extensions for advanced features
    #[serde(rename = "_rift", skip_serializing_if = "Option::is_none")]
    pub rift: Option<RiftResponseExtension>,
    /// Flat / recorded response form (issue #304): `statusCode`/`headers`/`body`/`_mode` at the
    /// top level with no `is` wrapper (the shape emitted by recorded/Mimeo-solo mocks). Mountebank
    /// renders these exactly like `is: { … }`. `is` still takes precedence when both are present.
    #[serde(default, deserialize_with = "deserialize_optional_status_code")]
    pub status_code: Option<u16>,
    #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(rename = "_mode", default)]
    pub mode: ResponseMode,
}

/// Serialization type for stub responses - outputs Mountebank-compatible format
/// Uses `behaviors` as array (Mountebank standard format)
/// Field ordering matches Mountebank: behaviors, is, proxy
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StubResponseOut {
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

/// Raw IsResponse that handles statusCode as string or number (for deserialization)
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IsResponseRaw {
    #[serde(
        default = "default_status_code",
        deserialize_with = "deserialize_status_code"
    )]
    pub status_code: u16,
    #[serde(default, deserialize_with = "multi_value_headers::deserialize")]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    #[serde(rename = "_mode", default)]
    pub mode: ResponseMode,
}

/// IsResponse for serialization - outputs statusCode as string (Mountebank format)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IsResponseOut {
    /// Status code serialized as string for Mountebank compatibility
    #[serde(serialize_with = "serialize_status_code_as_string")]
    pub status_code: u16,
    #[serde(default, serialize_with = "multi_value_headers::serialize")]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    /// Skipped when text (default) as Mountebank doesn't output it for text mode
    #[serde(rename = "_mode", default, skip_serializing_if = "is_text_mode")]
    pub mode: ResponseMode,
}

/// Serialize statusCode as a string for Mountebank compatibility
fn serialize_status_code_as_string<S>(status_code: &u16, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&status_code.to_string())
}

pub(crate) fn default_status_code() -> u16 {
    200
}

/// Parse a JSON `statusCode` value that may be a number or a (numeric) string.
fn parse_status_code_value<E: serde::de::Error>(value: serde_json::Value) -> Result<u16, E> {
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| E::custom("invalid status code number")),
        serde_json::Value::String(s) => s
            .parse::<u16>()
            .map_err(|_| E::custom(format!("invalid status code string: {s}"))),
        _ => Err(E::custom("statusCode must be a number or string")),
    }
}

/// Deserialize statusCode from either a number or a string
pub(crate) fn deserialize_status_code<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    parse_status_code_value(serde_json::Value::deserialize(deserializer)?)
}

/// Deserialize an optional top-level `statusCode` (flat response form, issue #304), reusing the
/// number-or-string parsing. Only invoked when the field is present; a `null` is treated as
/// absent (`None`) so a stray null on a non-flat response stays accepted as before.
pub(crate) fn deserialize_optional_status_code<'de, D>(
    deserializer: D,
) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Null => Ok(None),
        value => parse_status_code_value(value).map(Some),
    }
}

impl From<StubResponseRaw> for StubResponse {
    fn from(raw: StubResponseRaw) -> Self {
        // Priority: is > proxy > inject > fault > rift-script-only
        if let Some(is_raw) = raw.is {
            // Merge behaviors: prefer _behaviors, fall back to behaviors
            let behaviors = raw.underscore_behaviors.or_else(|| {
                // Convert array format to object format if needed
                raw.behaviors.and_then(normalize_behaviors)
            });
            StubResponse::Is {
                is: IsResponse {
                    status_code: is_raw.status_code,
                    headers: is_raw.headers,
                    body: is_raw.body,
                    mode: is_raw.mode,
                },
                behaviors,
                rift: raw.rift,
            }
        } else if let Some(proxy) = raw.proxy {
            StubResponse::Proxy { proxy }
        } else if let Some(inject) = raw.inject {
            StubResponse::Inject { inject }
        } else if let Some(fault) = raw.fault {
            StubResponse::Fault { fault }
        } else if let Some(rift) = raw.rift {
            // Rift-only response (script generates the response)
            StubResponse::RiftScript { rift }
        } else if raw.status_code.is_some() || raw.body.is_some() || !raw.headers.is_empty() {
            // Flat / recorded response form (issue #304): top-level statusCode/headers/body with
            // no `is` wrapper is rendered exactly like `is: { … }`. statusCode defaults to 200.
            let behaviors = raw
                .underscore_behaviors
                .or_else(|| raw.behaviors.and_then(normalize_behaviors));
            StubResponse::Is {
                is: IsResponse {
                    status_code: raw.status_code.unwrap_or_else(default_status_code),
                    headers: raw.headers,
                    body: raw.body,
                    mode: raw.mode,
                },
                behaviors,
                rift: None,
            }
        } else {
            // Default to empty Is response
            StubResponse::Is {
                is: IsResponse {
                    status_code: 200,
                    headers: HashMap::new(),
                    body: None,
                    mode: ResponseMode::Text,
                },
                behaviors: None,
                rift: None,
            }
        }
    }
}

impl From<StubResponse> for StubResponseOut {
    fn from(response: StubResponse) -> Self {
        match response {
            StubResponse::Is {
                is,
                behaviors,
                rift,
            } => StubResponseOut {
                is: Some(IsResponseOut {
                    status_code: is.status_code,
                    headers: is.headers,
                    body: is.body,
                    mode: is.mode,
                }),
                proxy: None,
                inject: None,
                fault: None,
                // Convert behaviors object to array format for Mountebank compatibility
                behaviors: behaviors.and_then(behaviors_to_array),
                rift,
            },
            StubResponse::Proxy { proxy } => StubResponseOut {
                is: None,
                proxy: Some(proxy),
                inject: None,
                fault: None,
                behaviors: None,
                rift: None,
            },
            StubResponse::Inject { inject } => StubResponseOut {
                is: None,
                proxy: None,
                inject: Some(inject),
                fault: None,
                behaviors: None,
                rift: None,
            },
            StubResponse::Fault { fault } => StubResponseOut {
                is: None,
                proxy: None,
                inject: None,
                fault: Some(fault),
                behaviors: None,
                rift: None,
            },
            StubResponse::RiftScript { rift } => StubResponseOut {
                is: None,
                proxy: None,
                inject: None,
                fault: None,
                behaviors: None,
                rift: Some(rift),
            },
        }
    }
}

/// Convert behaviors from object format to array format for Mountebank compatibility
/// Mountebank outputs: `"behaviors": [{"wait": ...}, {"decorate": ...}]`
/// Rift internally stores as object: `{"wait": ..., "decorate": ...}`
fn behaviors_to_array(value: serde_json::Value) -> Option<Vec<serde_json::Value>> {
    match value {
        serde_json::Value::Object(obj) => {
            if obj.is_empty() {
                None
            } else {
                // Convert each key-value pair to a separate object in the array
                let arr: Vec<serde_json::Value> = obj
                    .into_iter()
                    .map(|(k, v)| {
                        let mut m = serde_json::Map::new();
                        m.insert(k, v);
                        serde_json::Value::Object(m)
                    })
                    .collect();
                Some(arr)
            }
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                None
            } else {
                Some(arr)
            }
        }
        _ => None,
    }
}

/// Normalize behaviors from array format to object format
/// Some tools use `behaviors: [{"wait": ...}, {"decorate": ...}]` instead of
/// `_behaviors: {"wait": ..., "decorate": ...}`
pub(crate) fn normalize_behaviors(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Array(arr) => {
            // Convert array of behavior objects to a single merged object
            let mut merged = serde_json::Map::new();
            for item in arr {
                if let serde_json::Value::Object(obj) = item {
                    for (k, v) in obj {
                        merged.insert(k, v);
                    }
                }
            }
            if merged.is_empty() {
                None
            } else {
                Some(serde_json::Value::Object(merged))
            }
        }
        serde_json::Value::Object(_) => Some(value),
        _ => None,
    }
}

/// Response mode for body handling (Mountebank compatible)
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseMode {
    /// Body is UTF-8 text (default)
    #[default]
    Text,
    /// Body is base64-encoded binary data
    Binary,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IsResponse {
    #[serde(default = "default_status_code")]
    pub status_code: u16,
    #[serde(default, with = "multi_value_headers")]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    /// Response mode: "text" (default) or "binary" (body is base64-encoded)
    #[serde(rename = "_mode", default, skip_serializing_if = "is_text_mode")]
    pub mode: ResponseMode,
}

fn is_text_mode(mode: &ResponseMode) -> bool {
    *mode == ResponseMode::Text
}

/// Path rewrite configuration for proxy responses (Mountebank compatible)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathRewrite {
    /// Pattern to match in the path (string to replace)
    pub from: String,
    /// Replacement string
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyResponse {
    pub to: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub predicate_generators: Vec<serde_json::Value>,
    #[serde(default)]
    pub add_wait_behavior: bool,
    #[serde(default)]
    pub inject_headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_decorate_behavior: Option<String>,
    /// Path rewrite configuration for transforming the request path before proxying
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_rewrite: Option<PathRewrite>,
}

// ============================================================================
// Imposter Config
// ============================================================================

fn default_protocol() -> String {
    "http".to_string()
}

/// Configuration for creating an imposter
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImposterConfig {
    /// Port for the imposter. If not specified, an available port will be auto-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Host/IP address to bind the imposter to. Defaults to "0.0.0.0" (all interfaces).
    /// Use "127.0.0.1" or "localhost" for local-only access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    /// Inline PEM certificate for `protocol: "https"` (Mountebank-compatible). Paired with `key`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cert: Option<String>,
    /// Inline PEM private key for `protocol: "https"` (Mountebank-compatible). Paired with `cert`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub record_requests: bool,
    /// Record which stub matched each request (Mountebank compatible)
    #[serde(default)]
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
    /// Allow CORS headers (Mountebank compatible)
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        alias = "allowCORS"
    )]
    pub allow_cors: bool,
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
}

/// Flow state configuration for Rift extensions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiftFlowStateConfig {
    /// Backend type: "inmemory" or "redis"
    #[serde(default = "default_flow_backend")]
    pub backend: String,
    /// Default TTL for state entries in seconds
    #[serde(default = "default_flow_ttl")]
    pub ttl_seconds: i64,
    /// Redis configuration (required when backend is "redis")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redis: Option<RiftRedisConfig>,
    /// Source for the correlation `flow_id`: `"imposter_port"` (default) or `"header:<Name>"`.
    /// Flattened directly under `flowState` (issue #266). Absent ⇒ `"imposter_port"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id_source: Option<String>,
}

fn default_flow_backend() -> String {
    "inmemory".to_string()
}

fn default_flow_ttl() -> i64 {
    300
}

impl Default for RiftFlowStateConfig {
    fn default() -> Self {
        Self {
            backend: default_flow_backend(),
            ttl_seconds: default_flow_ttl(),
            redis: None,
            flow_id_source: None,
        }
    }
}

/// Redis configuration for flow state
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiftRedisConfig {
    /// Redis connection URL
    pub url: String,
    /// Connection pool size
    #[serde(default = "default_redis_pool")]
    pub pool_size: usize,
    /// Key prefix for all flow state keys
    #[serde(default = "default_redis_prefix")]
    pub key_prefix: String,
}

fn default_redis_pool() -> usize {
    10
}

fn default_redis_prefix() -> String {
    "rift:".to_string()
}

/// Metrics configuration for Rift extensions
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[serde(rename_all = "camelCase")]
pub struct RiftScriptEngineConfig {
    /// Default script engine: "rhai", "lua", or "javascript"
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
#[serde(rename_all = "camelCase")]
pub struct RiftResponseExtension {
    /// Fault injection configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fault: Option<RiftFaultConfig>,
    /// Script-based response generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<RiftScriptConfig>,
}

/// Fault injection configuration for responses
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    pub tcp: Option<String>,
}

/// Latency fault configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

fn default_error_status() -> u16 {
    503
}

/// Script configuration for response generation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiftScriptConfig {
    /// Script engine: "rhai", "lua", or "javascript"
    #[serde(default = "default_script_engine")]
    pub engine: String,
    /// Inline script code
    pub code: String,
}

// ============================================================================
// Error Types
// ============================================================================

/// Error types for imposter management
#[derive(Debug, thiserror::Error)]
pub enum ImposterError {
    #[error("Port {0} is already in use")]
    PortInUse(u16),
    #[error("Imposter not found on port {0}")]
    NotFound(u16),
    #[error("Failed to bind port {0}: {1}")]
    BindError(u16, String),
    #[error("Invalid protocol: {0}")]
    InvalidProtocol(String),
    #[error("Stub index {0} out of bounds")]
    StubIndexOutOfBounds(usize),
    #[error("No stub with id '{0}'")]
    StubNotFound(String),
    #[error("A stub with id '{0}' already exists")]
    StubIdConflict(String),
    #[error("Failed to persist imposter: {0}")]
    PersistError(String),
    #[error("TLS configuration error: {0}")]
    Tls(String),
    #[error("backend error: {0:#}")]
    Backend(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Issue #266: `flowIdSource` is flat under `flowState` and survives a serialize round-trip.
    #[test]
    fn flat_flow_id_source_round_trips() {
        let fs: RiftFlowStateConfig = serde_json::from_value(json!({
            "backend": "inmemory",
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

    // Issue #266: an absent `flowIdSource` deserializes to None and is omitted on serialize.
    #[test]
    fn absent_flow_id_source_is_none_and_omitted() {
        let fs: RiftFlowStateConfig =
            serde_json::from_value(json!({ "backend": "inmemory" })).unwrap();
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

    #[test]
    fn multi_value_headers_serialize_single_as_string_many_as_array() {
        let mut headers = HashMap::new();
        headers.insert("X-One".to_string(), vec!["v".to_string()]);
        headers.insert(
            "Set-Cookie".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        let out = IsResponseOut {
            status_code: 200,
            headers,
            body: None,
            mode: ResponseMode::Text,
        };
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
        // Regression guard: a genuinely-empty `{}` response must remain a 200 empty Is.
        let r: StubResponse = serde_json::from_value(json!({})).unwrap();
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
        let stub_json = json!({
            "predicates": [{ "equals": { "path": "/test" } }]
        });

        let result: Result<Stub, _> = serde_json::from_value(stub_json);

        assert!(
            result.is_ok(),
            "Stub without responses field should deserialize with empty responses vec"
        );
        assert!(result.unwrap().responses.is_empty());
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

    #[test]
    fn test_stub_predicates_takes_precedence_over_rules() {
        let stub_json = json!({
            "predicates": [{ "equals": { "path": "/a" } }],
            "rules": [{ "equals": { "path": "/b" } }, { "equals": { "path": "/c" } }],
            "responses": []
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
            let wait = behaviors.as_ref().unwrap().get("wait").unwrap();
            // min != max → range object
            assert_eq!(wait.get("min").unwrap(), &json!(50u64));
            assert_eq!(wait.get("max").unwrap(), &json!(100u64));
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
            let wait = behaviors.as_ref().unwrap().get("wait").unwrap();
            assert_eq!(wait, &json!(0u64));
        } else {
            panic!("expected Is response");
        }
    }

    #[test]
    fn test_stub_recorded_from_roundtrip() {
        let stub_json = json!({
            "predicates": [],
            "responses": [],
            "recordedFrom": "http://upstream:8080"
        });
        let stub: Stub = serde_json::from_value(stub_json).unwrap();
        assert_eq!(stub.recorded_from.as_deref(), Some("http://upstream:8080"));
        let serialized = serde_json::to_value(&stub).unwrap();
        assert_eq!(serialized["recordedFrom"], json!("http://upstream:8080"));
    }
}
