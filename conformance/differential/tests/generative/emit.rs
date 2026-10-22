//! Turns a generated `(stubs, ops)` pair into a case: the same JSON a `cases/*.json` entry holds,
//! so a shrunk failure is pasted into `cases/generated.json` as it is printed.

use crate::vocab::{self, IMPOSTER, MbOp, MbSpec, apply_edits};
use serde_json::{Value, json};

fn admin(method: &str, path: &str, body: Option<Value>) -> Value {
    let mut step = json!({ "do": "admin", "method": method, "path": path });
    if let Some(body) = body {
        step["body"] = body;
    }
    step
}

fn stubs_path() -> String {
    format!("/imposters/{IMPOSTER}/stubs")
}

/// The case `{name, steps}`: create the upstream and the imposter with `initial`, then each op.
/// Every admin op is followed by `GET /imposters/4545`, so a stored difference shows at the op that
/// made it, not only at the end. `stubs` tracks the authored list: recorded stubs exist only on the
/// engines, and an index that misses is still a valid step, because both engines answer it.
#[must_use]
pub fn case(name: &str, initial: &[MbSpec], ops: &[MbOp]) -> Value {
    let mut stubs = initial.to_vec();
    let mut steps = vec![
        admin("POST", "/imposters", Some(vocab::upstream())),
        admin("POST", "/imposters", Some(vocab::imposter(&stubs))),
    ];
    let get = || admin("GET", &format!("/imposters/{IMPOSTER}"), None);
    for op in ops {
        let bound = |index: usize, len: usize| index % (len + 1);
        match op {
            MbOp::Request(shape) => {
                steps.push(vocab::request(shape));
                continue;
            }
            MbOp::AddStub { index, spec } => {
                let at = bound(*index, stubs.len());
                stubs.insert(at, spec.clone());
                steps.push(admin(
                    "POST",
                    &stubs_path(),
                    Some(json!({ "index": at, "stub": spec.to_json() })),
                ));
            }
            MbOp::DeleteStub(index) => {
                let at = bound(*index, stubs.len());
                if at < stubs.len() {
                    stubs.remove(at);
                }
                steps.push(admin("DELETE", &format!("{}/{at}", stubs_path()), None));
            }
            MbOp::ReplaceStub { index, spec } => {
                let at = bound(*index, stubs.len());
                if let Some(slot) = stubs.get_mut(at) {
                    *slot = spec.clone();
                }
                steps.push(admin(
                    "PUT",
                    &format!("{}/{at}", stubs_path()),
                    Some(spec.to_json()),
                ));
            }
            MbOp::ReplaceStubs(edits) => {
                apply_edits(&mut stubs, edits);
                let list: Vec<Value> = stubs.iter().map(MbSpec::to_json).collect();
                steps.push(admin("PUT", &stubs_path(), Some(json!({ "stubs": list }))));
            }
            MbOp::PutImposters {
                edits,
                upstream_first,
            } => {
                apply_edits(&mut stubs, edits);
                let (upstream, imposter) = (vocab::upstream(), vocab::imposter(&stubs));
                let imposters = if *upstream_first {
                    json!([upstream, imposter])
                } else {
                    json!([imposter, upstream])
                };
                steps.push(admin(
                    "PUT",
                    "/imposters",
                    Some(json!({ "imposters": imposters })),
                ));
            }
            MbOp::Recreate(edits) => {
                apply_edits(&mut stubs, edits);
                steps.push(admin("DELETE", &format!("/imposters/{IMPOSTER}"), None));
                steps.push(admin("POST", "/imposters", Some(vocab::imposter(&stubs))));
            }
            MbOp::ClearProxies => steps.push(admin(
                "DELETE",
                &format!("/imposters/{IMPOSTER}/savedProxyResponses"),
                None,
            )),
            MbOp::ClearRequests => steps.push(admin(
                "DELETE",
                &format!("/imposters/{IMPOSTER}/savedRequests"),
                None,
            )),
        }
        steps.push(get());
    }
    json!({ "name": name, "steps": steps })
}
