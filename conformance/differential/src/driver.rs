//! Runs one case on both engines and collects every difference.

use crate::canon;
use crate::case::{Case, Request, Step};
use crate::diff::{Difference, Observation, compare_json, compare_observations, unified};
use crate::engine::{Engine, Outgoing, free_ports, send};
use crate::ports::PortMap;
use serde_json::Value;
use std::collections::BTreeSet;

/// Logical ports handed to imposters a case creates without a `port` (the engine picks one).
const AUTO_PORT_BASE: u16 = 64000;

pub struct Engines {
    pub mountebank: Engine,
    pub rift: Engine,
}

#[derive(Debug, Default)]
pub struct CaseReport {
    pub name: String,
    pub differences: Vec<Difference>,
    /// Unified diffs of the canonical stored imposters that differ, for the failure message.
    pub stored_diffs: Vec<String>,
    /// Why the case could not be compared as written (port allocation, an unreadable imposter
    /// list or export, a failed cleanup). Any entry fails the case.
    pub errors: Vec<String>,
}

pub async fn run_case(client: &reqwest::Client, engines: &Engines, case: &Case) -> CaseReport {
    let mut report = CaseReport {
        name: case.name.clone(),
        ..CaseReport::default()
    };
    let logical: Vec<u16> = case.logical_ports().into_iter().collect();
    let actual = match free_ports(logical.len() * 2) {
        Ok(ports) => ports,
        Err(e) => {
            report.errors.push(format!("port allocation: {e}"));
            return report;
        }
    };
    let (mb_ports, rift_ports) = actual.split_at(logical.len());
    let mut maps = [
        PortMap::new(
            logical.iter().copied().zip(mb_ports.iter().copied()),
            engines.mountebank.admin_port,
        ),
        PortMap::new(
            logical.iter().copied().zip(rift_ports.iter().copied()),
            engines.rift.admin_port,
        ),
    ];
    let mut next_auto = AUTO_PORT_BASE;

    for (index, step) in case.steps.iter().enumerate() {
        let at = format!("step[{index}]");
        match step {
            Step::Admin { method, path, body } => {
                let (mut mb, mut rift) =
                    admin_both(client, engines, &maps, method, path, body.as_ref()).await;
                if creates_unported_imposter(method, path, body.as_ref()) {
                    for (map, observation) in maps.iter_mut().zip([&mut mb, &mut rift]) {
                        if let Some(port) = body_port(observation) {
                            map.insert(next_auto, port);
                            *observation = remap(observation, map);
                        }
                    }
                    next_auto += 1;
                }
                report.differences.extend(compare_observations(
                    &format!("{at}.admin"),
                    &canonical_admin(&mb),
                    &canonical_admin(&rift),
                    true,
                ));
            }
            Step::Send(step) => {
                let (port, request, times) = (&step.port, step.request(), &step.times);
                let outgoing = [
                    imposter_request(&maps[0], *port, &request),
                    imposter_request(&maps[1], *port, &request),
                ];
                for rep in 0..*times {
                    let at = if *times > 1 {
                        format!("{at}.rep[{rep}]")
                    } else {
                        at.clone()
                    };
                    let (mb, rift) = tokio::join!(
                        send(client, &outgoing[0], &maps[0]),
                        send(client, &outgoing[1], &maps[1]),
                    );
                    report
                        .differences
                        .extend(compare_observations(&at, &mb, &rift, false));
                }
            }
            Step::Concurrent(step) => {
                let (port, request, times) = (&step.port, step.request(), &step.times);
                let burst = |map: &PortMap| {
                    let outgoing = imposter_request(map, *port, &request);
                    let map = map.clone();
                    async move {
                        let sends = (0..*times).map(|_| send(client, &outgoing, &map));
                        let mut all = futures::future::join_all(sends).await;
                        all.sort_by_key(|o| format!("{o:?}"));
                        all
                    }
                };
                let (mb, rift) = tokio::join!(burst(&maps[0]), burst(&maps[1]));
                if mb.len() != rift.len() {
                    report.errors.push(format!(
                        "concurrent burst: {} answers from mountebank, {} from rift",
                        mb.len(),
                        rift.len()
                    ));
                }
                for (n, (m, r)) in mb.iter().zip(rift.iter()).enumerate() {
                    report.differences.extend(compare_observations(
                        &format!("{at}.sorted[{n}]"),
                        m,
                        r,
                        false,
                    ));
                }
            }
            Step::Reimport => {
                let (mb, rift) = tokio::join!(
                    reimport(client, &engines.mountebank, &maps[0]),
                    reimport(client, &engines.rift, &maps[1]),
                );
                let (mb, rift) = match (mb, rift) {
                    (Ok(mb), Ok(rift)) => (mb, rift),
                    (mb, rift) => {
                        for (which, result) in [("mountebank", mb), ("rift", rift)] {
                            if let Err(export) = result {
                                report.errors.push(format!(
                                    "{which}: ?replayable=true export is not JSON, nothing to \
                                     re-import: {export:?}"
                                ));
                            }
                        }
                        continue;
                    }
                };
                for (label, m, r) in [("export", &mb.0, &rift.0), ("import", &mb.1, &rift.1)] {
                    report.differences.extend(compare_observations(
                        &format!("{at}.admin.{label}"),
                        &canonical_admin(m),
                        &canonical_admin(r),
                        true,
                    ));
                }
            }
        }
    }

    compare_stored(client, engines, &maps, &mut report).await;

    let (mb, rift) = admin_both(client, engines, &maps, "DELETE", "/imposters", None).await;
    for (which, observation) in [("mountebank", mb), ("rift", rift)] {
        if !matches!(observation, Observation::Response { status: 200, .. }) {
            report.errors.push(format!(
                "{which}: DELETE /imposters failed: {observation:?}"
            ));
        }
    }
    report
}

/// After the sequence: every imposter either engine holds, plain and replayable.
async fn compare_stored(
    client: &reqwest::Client,
    engines: &Engines,
    maps: &[PortMap; 2],
    report: &mut CaseReport,
) {
    let (mb, rift) = admin_both(client, engines, maps, "GET", "/imposters", None).await;
    let mut ports = BTreeSet::new();
    for (which, observation) in [("mountebank", &mb), ("rift", &rift)] {
        // An unreadable list would leave nothing to compare and pass the case vacuously.
        let listed = match observation {
            Observation::Response { status: 200, .. } => json_body(observation)
                .and_then(|v| v.get("imposters").and_then(Value::as_array).cloned()),
            _ => None,
        };
        let Some(imposters) = listed else {
            report.errors.push(format!(
                "{which}: GET /imposters is not a 200 imposter list: {observation:?}"
            ));
            continue;
        };
        ports.extend(
            imposters
                .iter()
                .filter_map(|i| i.get("port").and_then(Value::as_u64))
                .filter_map(|p| u16::try_from(p).ok()),
        );
    }
    for port in ports {
        // Which stubs a proxy recorded is visible only in the plain form (`recordedFrom`, or
        // `_proxyResponseTime` on the response); the replayable form reuses that answer.
        let mut recorded = [BTreeSet::new(), BTreeSet::new()];
        for (query, label) in [("", "stored"), ("?replayable=true", "replayable")] {
            let path = format!("/imposters/{port}{query}");
            let (mb, rift) = admin_both(client, engines, maps, "GET", &path, None).await;
            if query.is_empty() {
                for (set, observation) in recorded.iter_mut().zip([&mb, &rift]) {
                    if let Some(document) = json_body(observation) {
                        *set = canon::recorded_stubs(&document);
                    }
                }
            }
            let at = format!("{label}[{port}]");
            match (
                stored_document(&mb, &recorded[0]),
                stored_document(&rift, &recorded[1]),
            ) {
                (Some(m), Some(r)) => {
                    let before = report.differences.len();
                    compare_json(&at, &m, &r, &mut report.differences);
                    if report.differences.len() > before {
                        report
                            .stored_diffs
                            .push(format!("{at}:\n{}", unified(&m, &r)));
                    }
                }
                _ => report
                    .differences
                    .extend(compare_observations(&at, &mb, &rift, true)),
            }
        }
    }
}

fn stored_document(observation: &Observation, recorded: &BTreeSet<usize>) -> Option<Value> {
    match observation {
        Observation::Response { status: 200, .. } => {
            json_body(observation).map(|v| canon::imposter(&v, recorded))
        }
        _ => None,
    }
}

fn json_body(observation: &Observation) -> Option<Value> {
    match observation {
        Observation::Response { body, .. } => serde_json::from_slice(body).ok(),
        Observation::Failed(_) => None,
    }
}

/// An admin answer with its JSON body in canonical form (non-JSON bodies pass through).
fn canonical_admin(observation: &Observation) -> Observation {
    match (observation, json_body(observation)) {
        (
            Observation::Response {
                status, headers, ..
            },
            Some(json),
        ) => Observation::Response {
            status: *status,
            headers: headers.clone(),
            body: serde_json::to_vec(&canon::admin_document(&json))
                .expect("a serde_json::Value always serialises"),
        },
        _ => observation.clone(),
    }
}

/// The same admin call to both engines, concurrently.
async fn admin_both(
    client: &reqwest::Client,
    engines: &Engines,
    maps: &[PortMap; 2],
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (Observation, Observation) {
    tokio::join!(
        admin(client, &engines.mountebank, &maps[0], method, path, body),
        admin(client, &engines.rift, &maps[1], method, path, body),
    )
}

async fn admin(
    client: &reqwest::Client,
    engine: &Engine,
    map: &PortMap,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Observation {
    let (headers, body) = match body {
        None => (Vec::new(), None),
        Some(body) => (
            vec![("Content-Type".to_string(), "application/json".to_string())],
            Some(encode_body(map, body)),
        ),
    };
    let outgoing = Outgoing {
        port: engine.admin_port,
        method: method.to_string(),
        path: map.forward_path(path),
        headers,
        body,
    };
    send(client, &outgoing, map).await
}

fn imposter_request(map: &PortMap, port: u16, request: &Request) -> Outgoing {
    Outgoing {
        port: map.actual(port),
        method: request.method.clone(),
        path: request.path.clone(),
        headers: request
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), map.forward_str(value)))
            .collect(),
        body: match request.body_of_size {
            Some(size) => Some(vec![b'x'; size]),
            None => request.body.as_ref().map(|body| encode_body(map, body)),
        },
    }
}

/// A string body is sent verbatim (port-mapped); any other value is serialised as JSON.
fn encode_body(map: &PortMap, body: &Value) -> Vec<u8> {
    match body {
        Value::String(text) => map.forward_str(text).into_bytes(),
        other => serde_json::to_vec(&map.forward_value(other))
            .expect("a serde_json::Value always serialises"),
    }
}

/// `GET /imposters?replayable=true`, then `PUT /imposters` (which replaces every imposter) with
/// the export. `Err` carries an export that is not JSON, so there is nothing to re-import.
async fn reimport(
    client: &reqwest::Client,
    engine: &Engine,
    map: &PortMap,
) -> Result<(Observation, Observation), Observation> {
    let export = admin(
        client,
        engine,
        map,
        "GET",
        "/imposters?replayable=true",
        None,
    )
    .await;
    let Some(document) = json_body(&export) else {
        return Err(export);
    };
    // The export comes back port-mapped to logical; `admin` maps it forward again.
    let import = admin(client, engine, map, "PUT", "/imposters", Some(&document)).await;
    Ok((export, import))
}

fn creates_unported_imposter(method: &str, path: &str, body: Option<&Value>) -> bool {
    method.eq_ignore_ascii_case("POST")
        && path == "/imposters"
        && body.is_some_and(|b| b.is_object() && b.get("port").is_none())
}

fn body_port(observation: &Observation) -> Option<u16> {
    json_body(observation)?
        .get("port")?
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
}

fn remap(observation: &Observation, map: &PortMap) -> Observation {
    match (observation, json_body(observation)) {
        (
            Observation::Response {
                status, headers, ..
            },
            Some(json),
        ) => Observation::Response {
            status: *status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.clone(), map.reverse_str(v)))
                .collect(),
            body: serde_json::to_vec(&map.reverse_value(&json))
                .expect("a serde_json::Value always serialises"),
        },
        _ => observation.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_engine_assigned_port_is_registered_and_mapped_back() {
        let body = json!({"protocol": "http"});
        assert!(creates_unported_imposter("POST", "/imposters", Some(&body)));
        assert!(!creates_unported_imposter(
            "POST",
            "/imposters",
            Some(&json!({"port": 1}))
        ));
        assert!(!creates_unported_imposter("PUT", "/imposters", Some(&body)));

        let created = Observation::Response {
            status: 201,
            headers: vec![("location".to_string(), "http://localhost:2525/imposters/51234".to_string())],
            body: br#"{"port":51234,"_links":{"self":{"href":"http://localhost:2525/imposters/51234"}}}"#.to_vec(),
        };
        assert_eq!(body_port(&created), Some(51234));
        let mut map = PortMap::new([], 50000);
        map.insert(AUTO_PORT_BASE, 51234);
        let Observation::Response { headers, body, .. } = remap(&created, &map) else {
            panic!("a response stays a response");
        };
        assert_eq!(headers[0].1, "http://localhost:2525/imposters/64000");
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("json"),
            json!({"port": 64000, "_links": {"self": {"href": "http://localhost:2525/imposters/64000"}}})
        );
        assert_eq!(
            map.forward_path("/imposters/64000/stubs"),
            "/imposters/51234/stubs"
        );
    }
}
