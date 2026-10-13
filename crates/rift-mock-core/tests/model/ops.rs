//! The operation vocabulary of the model test and its generators (issue #1344).
//!
//! Every value is drawn from a small closed set (two paths, two flows, two scenarios, …) so random
//! sequences collide often: a stub added by one op is what a later request hits, a recording is what
//! a later `ReplaceStubs` keeps or drops. Indices are plain small numbers taken modulo the live
//! length when applied, so every subsequence of a case is still a valid case and shrinking can drop
//! any op.

use proptest::prelude::*;
use proptest::sample::select;
use serde_json::{Map, Value, json};

pub const PATHS: [&str; 2] = ["/a", "/b"];
pub const FLOWS: [&str; 2] = ["f1", "f2"];
pub const SCENARIOS: [&str; 2] = ["S1", "S2"];
pub const STATES: [&str; 3] = ["Started", "A", "B"];
/// Raw flow-state keys holding numbers. Disjoint from [`SCENARIOS`]: a scenario's state lives in the
/// same store under its name, and a non-string written there is a documented error, not a state, so
/// a raw write to a scenario's key always writes a state.
pub const FLOW_KEYS: [&str; 2] = ["k1", "k2"];
pub const BODIES: [&str; 2] = ["x", "y"];
/// Response-body labels of authored stubs. Few on purpose: content-identical stubs are what the
/// reconcile's occurrence keys exist for.
pub const LABELS: [&str; 3] = ["p", "q", "r"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One request the test can send. `GET`s carry no body.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shape {
    pub method: Method,
    pub path: &'static str,
    /// Whether the query string is `?q=1`.
    pub query: bool,
    pub body: Option<&'static str>,
    /// The `X-Flow` header, which is the imposter's `flowIdSource`.
    pub flow: &'static str,
}

/// An authored stub's predicates: one `equals` over the fields that are set, or none at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pred {
    pub method: Option<Method>,
    pub path: Option<&'static str>,
    /// Requires `q=1` in the query.
    pub query: bool,
    pub body: Option<&'static str>,
}

impl Pred {
    pub fn matches(&self, r: &Shape) -> bool {
        self.method.is_none_or(|m| m == r.method)
            && self.path.is_none_or(|p| p == r.path)
            && (!self.query || r.query)
            && self.body.is_none_or(|b| r.body == Some(b))
    }

    pub fn to_json(&self) -> Value {
        let mut equals = Map::new();
        if let Some(m) = self.method {
            equals.insert("method".into(), json!(m.as_str()));
        }
        if let Some(p) = self.path {
            equals.insert("path".into(), json!(p));
        }
        if self.query {
            equals.insert("query".into(), json!({ "q": "1" }));
        }
        if let Some(b) = self.body {
            equals.insert("body".into(), json!(b));
        }
        if equals.is_empty() {
            json!([])
        } else {
            json!([{ "equals": equals }])
        }
    }
}

/// A stub's scenario fields. `required` gates it, `next` is the transition after it answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gate {
    pub scenario: &'static str,
    pub required: Option<&'static str>,
    pub next: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Once,
    Always,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Once => "proxyOnce",
            Mode::Always => "proxyAlways",
        }
    }
}

/// A stub the test authors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spec {
    /// `responses` `is` responses with bodies `<label>0`, `<label>1` and statuses 200, 201.
    /// `conditional` (`Last-Modified: load`) only on a single-response stub: cycling through
    /// responses under one load stamp is not a content change.
    Is {
        pred: Pred,
        gate: Option<Gate>,
        label: &'static str,
        responses: usize,
        conditional: bool,
    },
    /// A proxy to the case's upstream, generating predicates from the path, and the body too when
    /// `gen_body` (the #1333 shape).
    Proxy {
        pred: Pred,
        mode: Mode,
        gen_body: bool,
    },
}

/// The body and status of an `is` stub's `i`th response.
pub fn is_response(label: &str, i: usize) -> (u16, String) {
    (if i == 0 { 200 } else { 201 }, format!("{label}{i}"))
}

impl Spec {
    pub fn to_json(&self, upstream: &str) -> Value {
        match self {
            Spec::Is {
                pred,
                gate,
                label,
                responses,
                conditional,
            } => {
                let responses: Vec<Value> = (0..*responses)
                    .map(|i| {
                        let (status, body) = is_response(label, i);
                        let mut response = json!({ "is": { "statusCode": status, "body": body } });
                        if *conditional {
                            response["_rift"] = json!({
                                "conditional": { "etag": false, "lastModified": "load" }
                            });
                        }
                        response
                    })
                    .collect();
                let mut stub = json!({ "predicates": pred.to_json(), "responses": responses });
                if let Some(gate) = gate {
                    stub["scenarioName"] = json!(gate.scenario);
                    if let Some(required) = gate.required {
                        stub["requiredScenarioState"] = json!(required);
                    }
                    if let Some(next) = gate.next {
                        stub["newScenarioState"] = json!(next);
                    }
                }
                stub
            }
            Spec::Proxy {
                pred,
                mode,
                gen_body,
            } => {
                let mut matches = json!({ "path": true });
                if *gen_body {
                    matches["body"] = json!(true);
                }
                json!({
                    "predicates": pred.to_json(),
                    "responses": [{ "proxy": {
                        "to": upstream,
                        "mode": mode.as_str(),
                        "predicateGenerators": [{ "matches": matches }]
                    }}]
                })
            }
        }
    }
}

/// One edit of a stub list. `ReplaceStubs`, `Reload` and `Recreate` each apply a short list of these
/// to the stubs the imposter holds *now*, so they keep recordings and survivors the way a client
/// editing a fetched imposter would.
#[derive(Clone, Debug)]
pub enum Edit {
    Insert(usize, Spec),
    Remove(usize),
    Swap(usize, usize),
    Clear,
}

/// Apply `edits` to `list`, taking every index modulo the length it applies to.
pub fn apply_edits<T: Clone>(list: &mut Vec<T>, edits: &[Edit], make: &impl Fn(&Spec) -> T) {
    for edit in edits {
        match edit {
            Edit::Insert(i, spec) => {
                let at = i % (list.len() + 1);
                list.insert(at, make(spec));
            }
            Edit::Remove(i) if !list.is_empty() => {
                let at = i % list.len();
                list.remove(at);
            }
            Edit::Swap(i, j) if list.len() >= 2 => {
                let n = list.len();
                list.swap(i % n, j % n);
            }
            Edit::Clear => list.clear(),
            Edit::Remove(_) | Edit::Swap(..) => {}
        }
    }
}

#[derive(Clone, Debug)]
pub enum Op {
    Request(Shape),
    /// `PUT /imposters/:port/stubs` with the current stubs, edited.
    ReplaceStubs(Vec<Edit>),
    AddStub {
        index: usize,
        spec: Spec,
    },
    DeleteStub(usize),
    MoveStub(usize, usize),
    SetScenarioState {
        flow: &'static str,
        scenario: &'static str,
        state: &'static str,
    },
    ResetScenarios {
        flow: &'static str,
    },
    /// A raw flow-state write: a number under a plain key, or a state under a scenario's key.
    FlowPut {
        flow: &'static str,
        key: &'static str,
        value: Value,
    },
    FlowDelete {
        flow: &'static str,
        key: &'static str,
    },
    FlowClear {
        flow: &'static str,
    },
    /// A hot reload carrying the current stubs, edited: a reconcile against the live imposter.
    Reload(Vec<Edit>),
    /// `CreateImposter` again on the same port (delete, then create) with the current stubs, edited.
    Recreate(Vec<Edit>),
}

fn method() -> impl Strategy<Value = Method> {
    prop_oneof![3 => Just(Method::Get), 2 => Just(Method::Post)]
}

/// Mostly `f1`: interactions need two requests to share a flow.
fn flow() -> impl Strategy<Value = &'static str> {
    prop_oneof![3 => Just(FLOWS[0]), 1 => Just(FLOWS[1])]
}

pub fn shape() -> impl Strategy<Value = Shape> {
    (
        method(),
        select(&PATHS[..]),
        prop::bool::weighted(0.15),
        prop::option::of(select(&BODIES[..])),
        flow(),
    )
        .prop_map(|(method, path, query, body, flow)| Shape {
            method,
            path,
            query,
            body: if method == Method::Post { body } else { None },
            flow,
        })
}

fn pred() -> impl Strategy<Value = Pred> {
    (
        prop::option::weighted(0.2, method()),
        prop::option::weighted(0.6, select(&PATHS[..])),
        prop::bool::weighted(0.15),
        prop::option::weighted(0.2, select(&BODIES[..])),
    )
        .prop_map(|(method, path, query, body)| Pred {
            method,
            path,
            query,
            body,
        })
}

fn gate() -> impl Strategy<Value = Option<Gate>> {
    prop::option::weighted(
        0.4,
        (
            select(&SCENARIOS[..]),
            prop::option::of(select(&STATES[..])),
            prop::option::of(select(&STATES[..])),
        )
            .prop_map(|(scenario, required, next)| Gate {
                scenario,
                required,
                next,
            }),
    )
}

pub fn spec() -> impl Strategy<Value = Spec> {
    let responses = prop_oneof![3 => Just(1usize), 1 => Just(2usize)];
    let is = (
        pred(),
        gate(),
        select(&LABELS[..]),
        responses,
        prop::bool::weighted(0.6),
    )
        .prop_map(|(pred, gate, label, responses, conditional)| Spec::Is {
            pred,
            gate,
            label,
            responses,
            conditional: conditional && responses == 1,
        });
    let proxy = (
        // Mostly scoped to one path: a catch-all proxy shadows every stub behind it.
        prop::option::weighted(0.8, select(&PATHS[..])),
        prop_oneof![Just(Mode::Once), Just(Mode::Always)],
        any::<bool>(),
    )
        .prop_map(|(path, mode, gen_body)| Spec::Proxy {
            pred: Pred {
                path,
                ..Pred::default()
            },
            mode,
            gen_body,
        });
    prop_oneof![6 => is, 1 => proxy]
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    let edit = prop_oneof![
        3 => (0..8usize, spec()).prop_map(|(i, s)| Edit::Insert(i, s)),
        3 => (0..8usize).prop_map(Edit::Remove),
        2 => (0..8usize, 0..8usize).prop_map(|(i, j)| Edit::Swap(i, j)),
        1 => Just(Edit::Clear),
    ];
    prop::collection::vec(edit, 0..4)
}

pub fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => shape().prop_map(Op::Request),
        1 => edits().prop_map(Op::ReplaceStubs),
        3 => (0..8usize, spec()).prop_map(|(index, spec)| Op::AddStub { index, spec }),
        1 => (0..8usize).prop_map(Op::DeleteStub),
        1 => (0..8usize, 0..8usize).prop_map(|(from, to)| Op::MoveStub(from, to)),
        2 => (flow(), select(&SCENARIOS[..]), select(&STATES[..]))
            .prop_map(|(flow, scenario, state)| Op::SetScenarioState { flow, scenario, state }),
        1 => flow().prop_map(|flow| Op::ResetScenarios { flow }),
        1 => (flow(), select(&FLOW_KEYS[..]), 0..4i64)
            .prop_map(|(flow, key, n)| Op::FlowPut { flow, key, value: json!(n) }),
        1 => (flow(), select(&SCENARIOS[..]), select(&STATES[..]))
            .prop_map(|(flow, key, state)| Op::FlowPut { flow, key, value: json!(state) }),
        1 => (flow(), prop_oneof![select(&FLOW_KEYS[..]), select(&SCENARIOS[..])])
            .prop_map(|(flow, key)| Op::FlowDelete { flow, key }),
        1 => flow().prop_map(|flow| Op::FlowClear { flow }),
        1 => edits().prop_map(Op::Reload),
        1 => edits().prop_map(Op::Recreate),
    ]
}

/// A case: the stubs `CreateImposter` starts with, then the operations.
pub fn case() -> impl Strategy<Value = (Vec<Spec>, Vec<Op>)> {
    (
        prop::collection::vec(spec(), 0..6),
        prop::collection::vec(op(), 1..40),
    )
}
