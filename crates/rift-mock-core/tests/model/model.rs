//! The reference model (issue #1344): what one imposter should do, written as plainly as possible.
//!
//! Matching, proxy recording and response cycling follow Mountebank: the first stub, in order,
//! whose predicates match answers; a stub cycles through its responses; a `proxyOnce` miss is
//! forwarded and recorded as a stub in front of the proxy, a `proxyAlways` one behind it, merged into
//! an equal-predicate recording when there is one. Scenarios, raw flow state and reload reconcile
//! are Rift's own documented contracts (scenario state lives in the flow store under the scenario's
//! name; a reload patches the stub set in place unless more than half of it changes, and then
//! replaces the imposter and its state).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::ops::{Edit, Method, Mode, Shape, Spec, apply_edits, is_response};

/// Bodies the upstream answers per path, cycling. More than any case can reach.
pub const UPSTREAM_RESPONSES: usize = 64;

pub fn upstream_body(path: &str, hit: usize) -> String {
    format!("up{path}#{hit}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Authored(Spec),
    /// A recording: predicates generated from `path` (and from the body when `body` is `Some`),
    /// and the upstream responses recorded into it.
    Recorded {
        path: &'static str,
        body: Option<Option<&'static str>>,
        responses: Vec<String>,
    },
}

impl Kind {
    fn matches(&self, r: &Shape) -> bool {
        match self {
            Kind::Authored(Spec::Is { pred, .. } | Spec::Proxy { pred, .. }) => pred.matches(r),
            Kind::Recorded { path, body, .. } => {
                *path == r.path && body.is_none_or(|b| b == r.body)
            }
        }
    }

    /// What the engine's stub list should show for this stub; see `fingerprint` in `main.rs`.
    pub fn fingerprint(&self, upstream: &str) -> String {
        match self {
            Kind::Authored(Spec::Is {
                pred,
                gate,
                label,
                responses,
                ..
            }) => {
                let bodies: Vec<String> =
                    (0..*responses).map(|i| is_response(label, i).1).collect();
                let gate = gate.as_ref().map_or(String::new(), |g| {
                    format!(
                        " {}:{}->{}",
                        g.scenario,
                        g.required.unwrap_or("*"),
                        g.next.unwrap_or("=")
                    )
                });
                format!("is[{}]{gate} {}", bodies.join(","), pred.to_json())
            }
            Kind::Authored(Spec::Proxy { pred, mode, .. }) => {
                format!("proxy:{} {}", mode.as_str(), pred.to_json())
            }
            Kind::Recorded { responses, .. } => {
                format!("rec@{upstream}[{}]", responses.join(","))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Slot {
    pub kind: Kind,
    /// How many times this slot has answered: its next response is `cursor % responses`.
    cursor: usize,
    /// The `proxyOnce` claim this slot was recorded under. It travels with the slot (moves,
    /// reload survivors) and ends with it: when the slot goes, or its content changes, so does
    /// the claim (#1386).
    claim: Option<ClaimKey>,
}

impl Slot {
    fn new(kind: Kind) -> Self {
        Slot {
            kind,
            cursor: 0,
            claim: None,
        }
    }
}

/// What a reload does to the imposter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reload {
    Unchanged,
    /// The stub set is edited in place; the imposter and its state stay.
    Patched,
    /// More than half the stubs changed: the imposter is replaced and its state starts over.
    Replaced,
}

/// What a request should get back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Served {
    pub status: u16,
    pub body: String,
    /// Answered by a proxy stub (`x-rift-proxy: true`), not by a recorded or authored one.
    pub proxied: bool,
    /// Carries `Last-Modified` (a conditional stub answering a `GET`).
    pub last_modified: bool,
}

impl Served {
    /// A `200` without validators.
    fn plain(body: String, proxied: bool) -> Self {
        Served {
            status: 200,
            body,
            proxied,
            last_modified: false,
        }
    }
}

/// The `proxyOnce` replay key: method, path, query and the generated predicates (#1317, #1333).
type ClaimKey = (Method, &'static str, bool, Option<Option<&'static str>>);

#[derive(Debug, Default)]
pub struct Model {
    pub stubs: Vec<Slot>,
    /// `proxyOnce` recordings by identity; only a `proxyOnce` stub consults or fills them. Each
    /// lives exactly as long as the stub it was recorded as (#1386): with that stub deleted or
    /// replaced, the next request for the identity is forwarded and recorded again, as in
    /// Mountebank, where the stub is the recording.
    claims: BTreeMap<ClaimKey, String>,
    /// Raw flow state, keyed `(flow, key)`: the `FLOW_KEYS` values and the scenario states.
    pub kv: BTreeMap<(&'static str, &'static str), Value>,
    /// Upstream requests per path.
    pub hits: BTreeMap<&'static str, usize>,
}

impl Model {
    pub fn new(specs: &[Spec]) -> Self {
        let mut model = Model::default();
        model.recreate_with(specs.iter().cloned().map(Kind::Authored).collect());
        model
    }

    pub fn upstream_hits(&self) -> usize {
        self.hits.values().sum()
    }

    fn scenario_state(&self, flow: &'static str, scenario: &'static str) -> &str {
        match self.kv.get(&(flow, scenario)) {
            Some(Value::String(s)) => s,
            Some(other) => panic!("model wrote a non-string scenario state: {other}"),
            None => "Started",
        }
    }

    fn eligible(&self, slot: &Slot, r: &Shape) -> bool {
        if let Kind::Authored(Spec::Is {
            gate: Some(gate), ..
        }) = &slot.kind
            && let Some(required) = gate.required
            && self.scenario_state(r.flow, gate.scenario) != required
        {
            return false;
        }
        slot.kind.matches(r)
    }

    pub fn request(&mut self, r: &Shape) -> Served {
        let Some(i) = self.stubs.iter().position(|s| self.eligible(s, r)) else {
            return Served::plain(String::new(), false);
        };
        let slot = &mut self.stubs[i];
        let turn = slot.cursor;
        slot.cursor += 1;
        // Cloned so the arms below can mutate `self` (transitions, recordings).
        match slot.kind.clone() {
            Kind::Authored(Spec::Is {
                gate,
                label,
                responses,
                conditional,
                ..
            }) => {
                if let Some(gate) = gate
                    && let Some(next) = gate.next
                {
                    self.kv.insert((r.flow, gate.scenario), json!(next));
                }
                let (status, body) = is_response(label, turn % responses);
                Served {
                    status,
                    body,
                    proxied: false,
                    last_modified: conditional && r.method == Method::Get,
                }
            }
            Kind::Recorded { responses, .. } => {
                Served::plain(responses[turn % responses.len()].clone(), false)
            }
            Kind::Authored(Spec::Proxy { mode, gen_body, .. }) => {
                let key = (r.method, r.path, r.query, gen_body.then_some(r.body));
                if mode == Mode::Once
                    && let Some(body) = self.claims.get(&key)
                {
                    return Served::plain(body.clone(), true);
                }
                let body = self.forward(r);
                let claim = (mode == Mode::Once).then_some(key);
                if let Some(key) = claim {
                    self.claims.insert(key, body.clone());
                }
                self.record(i, r, mode, gen_body, body.clone(), claim);
                self.drop_orphaned_claims();
                Served::plain(body, true)
            }
        }
    }

    fn forward(&mut self, r: &Shape) -> String {
        let hits = self.hits.entry(r.path).or_insert(0);
        let body = upstream_body(r.path, *hits % UPSTREAM_RESPONSES);
        *hits += 1;
        body
    }

    /// Record a forwarded response next to the proxy stub at `anchor`, the one that answered: a
    /// `proxyOnce` recording goes in front of it; a `proxyAlways` one merges into the first stub
    /// after it recorded with the same predicates, else goes at the end of the list.
    fn record(
        &mut self,
        anchor: usize,
        r: &Shape,
        mode: Mode,
        gen_body: bool,
        response: String,
        claim: Option<ClaimKey>,
    ) {
        let path = r.path;
        let body = gen_body.then_some(r.body);
        let recorded = |responses| {
            Slot::new(Kind::Recorded {
                path,
                body,
                responses,
            })
        };
        match mode {
            Mode::Once => self.stubs.insert(
                anchor,
                Slot {
                    claim,
                    ..recorded(vec![response])
                },
            ),
            Mode::Always => {
                let existing = self.stubs.iter_mut().skip(anchor + 1).find(|s| {
                    matches!(&s.kind, Kind::Recorded { path: p, body: b, .. } if *p == path && *b == body)
                });
                match existing {
                    // New content: whatever claim the stub was recorded under ends here.
                    Some(Slot {
                        kind: Kind::Recorded { responses, .. },
                        claim,
                        ..
                    }) => {
                        responses.push(response);
                        *claim = None;
                    }
                    _ => self.stubs.push(recorded(vec![response])),
                }
            }
        }
    }

    fn edited(&self, edits: &[Edit]) -> Vec<Kind> {
        let mut kinds: Vec<Kind> = self.stubs.iter().map(|s| s.kind.clone()).collect();
        apply_edits(&mut kinds, edits, &|spec: &Spec| {
            Kind::Authored(spec.clone())
        });
        kinds
    }

    /// `PUT /imposters/:port/stubs`: every slot is new, so every response cycle restarts.
    pub fn replace_stubs(&mut self, edits: &[Edit]) {
        self.stubs = self.edited(edits).into_iter().map(Slot::new).collect();
        self.drop_orphaned_claims();
    }

    /// Where the stub went.
    pub fn add_stub(&mut self, index: usize, spec: &Spec) -> usize {
        let at = index % (self.stubs.len() + 1);
        self.stubs
            .insert(at, Slot::new(Kind::Authored(spec.clone())));
        at
    }

    /// `None` when there is no stub to delete — the engine must refuse.
    pub fn delete_stub(&mut self, index: usize) -> Option<usize> {
        if self.stubs.is_empty() {
            return None;
        }
        let at = index % self.stubs.len();
        self.stubs.remove(at);
        self.drop_orphaned_claims();
        Some(at)
    }

    /// `None` when there is no stub to move — the engine must refuse.
    pub fn move_stub(&mut self, from: usize, to: usize) -> Option<(usize, usize)> {
        if self.stubs.is_empty() {
            return None;
        }
        let n = self.stubs.len();
        let (from, to) = (from % n, to % n);
        let slot = self.stubs.remove(from);
        self.stubs.insert(to, slot);
        Some((from, to))
    }

    pub fn set_scenario_state(&mut self, flow: &'static str, scenario: &'static str, state: &str) {
        self.kv.insert((flow, scenario), json!(state));
    }

    /// Resets the scenarios the current stubs declare, for one flow.
    pub fn reset_scenarios(&mut self, flow: &'static str) {
        let names: Vec<&'static str> = self
            .stubs
            .iter()
            .filter_map(|s| match &s.kind {
                Kind::Authored(Spec::Is {
                    gate: Some(gate), ..
                }) => Some(gate.scenario),
                _ => None,
            })
            .collect();
        for name in names {
            self.kv.remove(&(flow, name));
        }
    }

    pub fn flow_put(&mut self, flow: &'static str, key: &'static str, value: Value) {
        self.kv.insert((flow, key), value);
    }

    pub fn flow_delete(&mut self, flow: &'static str, key: &'static str) {
        self.kv.remove(&(flow, key));
    }

    /// Clears every key of the flow, scenario states included: they share the store.
    pub fn flow_clear(&mut self, flow: &'static str) {
        self.kv.retain(|(f, _), _| *f != flow);
    }

    /// A hot reload toward the edited stub list. Stubs are keyed by content and occurrence; the
    /// surviving keys keep their slot (and response cycle) unless more than half of both sides
    /// changes, in which case the imposter is replaced and its state starts over.
    pub fn reload(&mut self, edits: &[Edit]) -> Reload {
        let desired = self.edited(edits);
        let old_keys = keys(self.stubs.iter().map(|s| &s.kind));
        let new_keys = keys(desired.iter());
        let deletes = old_keys.iter().filter(|k| !new_keys.contains(k)).count();
        let inserts = new_keys.iter().filter(|k| !old_keys.contains(k)).count();
        if old_keys == new_keys {
            return Reload::Unchanged;
        }
        if (deletes + inserts) * 2 > old_keys.len() + new_keys.len() {
            self.recreate_with(desired);
            return Reload::Replaced;
        }
        let mut old: Vec<Option<Slot>> = self.stubs.drain(..).map(Some).collect();
        self.stubs = desired
            .into_iter()
            .zip(&new_keys)
            .map(|(kind, key)| {
                old_keys
                    .iter()
                    .position(|k| k == key)
                    .and_then(|j| old[j].take())
                    .unwrap_or_else(|| Slot::new(kind))
            })
            .collect();
        self.drop_orphaned_claims();
        Reload::Patched
    }

    /// `CreateImposter` on the same port: new slots and an empty flow store.
    pub fn recreate(&mut self, edits: &[Edit]) {
        let desired = self.edited(edits);
        self.recreate_with(desired);
    }

    /// Forget every claim no remaining slot was recorded under.
    fn drop_orphaned_claims(&mut self) {
        let held: Vec<ClaimKey> = self.stubs.iter().filter_map(|s| s.claim).collect();
        self.claims.retain(|key, _| held.contains(key));
    }

    /// A new imposter: new slots, an empty flow store and an empty proxy store.
    fn recreate_with(&mut self, desired: Vec<Kind>) {
        self.claims.clear();
        self.stubs = desired.into_iter().map(Slot::new).collect();
        self.kv.clear();
    }

    pub fn fingerprints(&self, upstream: &str) -> Vec<String> {
        self.stubs
            .iter()
            .map(|s| s.kind.fingerprint(upstream))
            .collect()
    }
}

/// A reconcile key per stub: its content and how many content-equal stubs precede it.
fn keys<'a>(kinds: impl Iterator<Item = &'a Kind>) -> Vec<(Kind, usize)> {
    let mut seen: Vec<(Kind, usize)> = Vec::new();
    kinds
        .map(|kind| {
            let occurrence = seen.iter().filter(|(k, _)| k == kind).count();
            seen.push((kind.clone(), occurrence));
            (kind.clone(), occurrence)
        })
        .collect()
}
