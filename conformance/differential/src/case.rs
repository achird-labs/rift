//! The case corpus: `cases/*.json` files, plus the Mountebank-drivable SDK corpus fixtures.
//!
//! A case is a sequence of steps sent to both engines. It carries no expectations — the other
//! engine's answer is the expectation.

use crate::ports::collect_ports;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;

/// One `cases/*.json` file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseFile {
    /// Where the cases came from (a retired feature file, an issue, the SDK corpus).
    pub source: String,
    #[serde(default)]
    pub cases: Vec<Case>,
    /// SDK-corpus fixtures replayed as cases; see [`FixtureRef`].
    #[serde(default)]
    pub fixtures: Vec<FixtureRef>,
    /// SDK-corpus fixtures deliberately not driven, each with the reason Mountebank cannot run it.
    #[serde(default, rename = "notDriven")]
    pub not_driven: Vec<NotDriven>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotDriven {
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "do", rename_all = "camelCase", deny_unknown_fields)]
pub enum Step {
    /// A call to the admin API of both engines. A JSON `body` is serialised; a string `body` is
    /// sent verbatim (malformed-JSON cases).
    Admin {
        method: String,
        path: String,
        #[serde(default)]
        body: Option<Value>,
    },
    /// A request to imposter `port` on both engines, `times` times in a row.
    Send(SendStep),
    /// `times` concurrent requests; the multiset of answers is compared, not their order.
    Concurrent(SendStep),
    /// Each engine exports `GET /imposters?replayable=true`, deletes everything and re-imports
    /// its own export with `PUT /imposters`.
    Reimport,
}

/// The fields of a `send` / `concurrent` step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendStep {
    pub port: u16,
    #[serde(default = "get")]
    pub method: String,
    #[serde(default = "root")]
    pub path: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default, rename = "bodyOfSize")]
    pub body_of_size: Option<usize>,
    #[serde(default = "one")]
    pub times: u32,
}

impl SendStep {
    #[must_use]
    pub fn request(&self) -> Request {
        Request {
            method: self.method.clone(),
            path: self.path.clone(),
            headers: self.headers.clone(),
            body: self.body.clone(),
            body_of_size: self.body_of_size,
        }
    }
}

/// One request, as sent to an imposter.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Ordered `[name, value]` pairs, so duplicates and order survive.
    pub headers: Vec<(String, String)>,
    /// A string is sent verbatim; any other JSON value is serialised.
    pub body: Option<Value>,
    /// Instead of `body`: that many bytes of `x` (a large-body case without a 100 KB literal).
    pub body_of_size: Option<usize>,
}

fn one() -> u32 {
    1
}
fn get() -> String {
    "GET".to_string()
}
fn root() -> String {
    "/".to_string()
}

impl Case {
    /// Every logical port the case refers to.
    #[must_use]
    pub fn logical_ports(&self) -> BTreeSet<u16> {
        let mut ports = BTreeSet::new();
        for step in &self.steps {
            match step {
                Step::Admin { path, body, .. } => {
                    if let Some(port) = crate::ports::path_port(path) {
                        ports.insert(port);
                    }
                    if let Some(body) = body {
                        collect_ports(body, &mut ports);
                    }
                }
                Step::Send(send) | Step::Concurrent(send) => {
                    ports.insert(send.port);
                    for (_, value) in &send.headers {
                        collect_ports(&Value::String(value.clone()), &mut ports);
                    }
                }
                Step::Reimport => {}
            }
        }
        ports
    }
}

/// A fixture of `sdk-conformance/corpus/imposters/` replayed as a case: the fixture (and any
/// imposter it proxies to, `with`) is created on both engines with `_verify` stripped, then every
/// `_verify.sequence[].request` of every stub is sent to it in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FixtureRef {
    pub file: String,
    /// Fixtures that must exist first (a proxy fixture's upstream).
    #[serde(default)]
    pub with: Vec<String>,
    /// `dropStubs`: stubs (by `name`) Mountebank cannot run, removed before loading, each with the reason.
    #[serde(default)]
    pub drop_stubs: Vec<DroppedStub>,
    /// `dropKeys`: top-level keys removed before loading (a Rift-only `_rift` block the dropped stubs needed).
    #[serde(default)]
    pub drop_keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DroppedStub {
    pub name: String,
    pub reason: String,
}

/// Errors loading the corpus. Each names the file, so a broken case is never silently skipped.
#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

/// Turns a fixture reference into a case, reading the fixtures from `imposters_dir`.
///
/// # Errors
/// A missing or malformed fixture, a `drop_stubs` name that matches no stub, or a fixture with no
/// `_verify` request and no stubs (it would test nothing).
pub fn fixture_case(imposters_dir: &Path, fixture: &FixtureRef) -> Result<Case, LoadError> {
    let mut steps = Vec::new();
    for upstream in &fixture.with {
        let (imposter, _) = load_fixture(imposters_dir, upstream, &[], &[])?;
        steps.push(create(imposter));
    }
    let (imposter, requests) = load_fixture(
        imposters_dir,
        &fixture.file,
        &fixture.drop_stubs,
        &fixture.drop_keys,
    )?;
    let port = imposter
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| LoadError(format!("{}: fixture has no port", fixture.file)))?;
    steps.push(create(imposter));
    for request in requests {
        steps.push(Step::Send(SendStep {
            port,
            method: request.method,
            path: request.path,
            headers: request.headers,
            body: request.body,
            body_of_size: None,
            times: 1,
        }));
    }
    Ok(Case {
        name: format!("sdk-corpus: {}", fixture.file),
        steps,
    })
}

fn create(imposter: Value) -> Step {
    Step::Admin {
        method: "POST".to_string(),
        path: "/imposters".to_string(),
        body: Some(imposter),
    }
}

fn load_fixture(
    dir: &Path,
    file: &str,
    drop_stubs: &[DroppedStub],
    drop_keys: &[String],
) -> Result<(Value, Vec<Request>), LoadError> {
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| LoadError(format!("{}: {e}", path.display())))?;
    let mut imposter: Map<String, Value> =
        serde_json::from_str(&text).map_err(|e| LoadError(format!("{}: {e}", path.display())))?;
    for key in drop_keys {
        if imposter.remove(key).is_none() {
            return Err(LoadError(format!(
                "{file}: dropKeys names absent key {key:?}"
            )));
        }
    }
    let stubs = match imposter.remove("stubs") {
        Some(Value::Array(stubs)) => stubs,
        None => Vec::new(),
        Some(_) => return Err(LoadError(format!("{file}: stubs is not an array"))),
    };
    let mut kept = Vec::new();
    let mut requests = Vec::new();
    let mut dropped = 0;
    for mut stub in stubs {
        let name = stub.get("name").and_then(Value::as_str).unwrap_or_default();
        if drop_stubs.iter().any(|d| d.name == name) {
            dropped += 1;
            continue;
        }
        if let Some(verify) = stub.as_object_mut().and_then(|s| s.remove("_verify")) {
            requests.extend(verify_requests(file, &verify)?);
        }
        kept.push(stub);
    }
    if dropped != drop_stubs.len() {
        return Err(LoadError(format!(
            "{file}: dropStubs names {} stubs but {dropped} matched",
            drop_stubs.len()
        )));
    }
    if kept.is_empty() {
        return Err(LoadError(format!("{file}: no stubs left to compare")));
    }
    imposter.insert("stubs".to_string(), Value::Array(kept));
    Ok((Value::Object(imposter), requests))
}

fn verify_requests(file: &str, verify: &Value) -> Result<Vec<Request>, LoadError> {
    let Some(sequence) = verify.get("sequence").and_then(Value::as_array) else {
        return Err(LoadError(format!(
            "{file}: _verify without a sequence array"
        )));
    };
    sequence
        .iter()
        .map(|entry| {
            let raw = entry.get("request").cloned().unwrap_or(Value::Null);
            let mut request: VerifyRequest = serde_json::from_value(raw)
                .map_err(|e| LoadError(format!("{file}: _verify request: {e}")))?;
            Ok(Request {
                method: request.method.take().unwrap_or_else(get),
                path: request.path,
                headers: request.headers.into_iter().collect(),
                body: request.body,
                body_of_size: None,
            })
        })
        .collect()
}

/// The `_verify` request shape (headers are an object there, not pairs).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyRequest {
    #[serde(default)]
    method: Option<String>,
    path: String,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    body: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_parse_from_the_case_schema() {
        let case: Case = serde_json::from_value(json!({
            "name": "n",
            "steps": [
                {"do": "admin", "method": "POST", "path": "/imposters", "body": {"port": 4545, "protocol": "http"}},
                {"do": "send", "port": 4545, "path": "/x", "headers": [["X-A", "1"]], "times": 2},
                {"do": "concurrent", "port": 4545, "times": 5},
                {"do": "reimport"}
            ]
        }))
        .expect("case parses");
        assert_eq!(case.steps.len(), 4);
        let Step::Send(send) = &case.steps[1] else {
            panic!("second step is a send");
        };
        assert_eq!(
            (send.method.as_str(), send.path.as_str(), send.times),
            ("GET", "/x", 2)
        );
        assert_eq!(send.headers, vec![("X-A".to_string(), "1".to_string())]);
        assert_eq!(
            case.logical_ports().into_iter().collect::<Vec<_>>(),
            vec![4545]
        );
    }

    #[test]
    fn unknown_keys_are_refused_at_every_level() {
        assert!(serde_json::from_value::<CaseFile>(json!({"source": "s", "case": []})).is_err());
        assert!(
            serde_json::from_value::<Case>(json!({"name": "n", "steps": [], "stpes": []})).is_err()
        );
        assert!(
            serde_json::from_value::<FixtureRef>(json!({"file": "f.json", "drop_stubs": []}))
                .is_err(),
            "the key is dropStubs"
        );
    }

    #[test]
    fn a_fixture_reference_that_has_rotted_is_an_error() {
        let dir =
            std::env::temp_dir().join(format!("rift-differential-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("f.json"),
            r#"{"port": 4501, "protocol": "http", "stubs": [{"name": "a", "responses": [{"is": {}}],
                "_verify": {"sequence": [{"request": {"path": "/a"}}]}}]}"#,
        )
        .expect("write fixture");
        let base = FixtureRef {
            file: "f.json".to_string(),
            with: vec![],
            drop_stubs: vec![],
            drop_keys: vec![],
        };

        let case = fixture_case(&dir, &base).expect("fixture loads");
        assert_eq!(case.steps.len(), 2, "create + one _verify request");
        let Step::Admin {
            body: Some(body), ..
        } = &case.steps[0]
        else {
            panic!("create step")
        };
        assert!(
            body.pointer("/stubs/0/_verify").is_none(),
            "_verify is stripped"
        );

        let unknown_stub = FixtureRef {
            drop_stubs: vec![DroppedStub {
                name: "b".to_string(),
                reason: "r".to_string(),
            }],
            ..base.clone()
        };
        assert!(
            fixture_case(&dir, &unknown_stub).is_err(),
            "dropStubs naming no stub"
        );
        let unknown_key = FixtureRef {
            drop_keys: vec!["_rift".to_string()],
            ..base.clone()
        };
        assert!(
            fixture_case(&dir, &unknown_key).is_err(),
            "dropKeys naming an absent key"
        );
        let everything = FixtureRef {
            drop_stubs: vec![DroppedStub {
                name: "a".to_string(),
                reason: "r".to_string(),
            }],
            ..base
        };
        assert!(fixture_case(&dir, &everything).is_err(), "no stubs left");
        std::fs::remove_dir_all(&dir).expect("remove temp dir");
    }

    #[test]
    fn an_unknown_step_key_is_refused() {
        let parsed = serde_json::from_value::<Case>(json!({
            "name": "n", "steps": [{"do": "send", "port": 1, "pth": "/typo"}]
        }));
        assert!(parsed.is_err(), "a misspelt key must not silently default");
    }
}
