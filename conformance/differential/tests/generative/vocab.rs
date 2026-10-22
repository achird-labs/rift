//! The Mountebank-drivable vocabulary (issue #1549): what a generated case may do, and what it
//! deliberately leaves out.
//!
//! Requests and authored stubs come from the model test's generators (`ops::shape()`,
//! `ops::spec()`); [`mb_spec`] drops their Rift-only fields and adds Mountebank decoration
//! (predicate operator, `repeat`, behaviors) drawn by strategies of its own, so each shrinks on its
//! own. The admin operations are the nine Mountebank has a route for.

use crate::ops::{self, Method, Mode, Pred, Shape, Spec};
use proptest::prelude::*;
use serde_json::{Map, Value, json};

/// The imposter every generated case drives.
pub const IMPOSTER: u16 = 4545;
/// The upstream its proxy stubs forward to: one stub per path, cycling `up/<path>#<k>`.
pub const UPSTREAM: u16 = 4546;
/// How many answers each upstream stub cycles through.
const UPSTREAM_ANSWERS: usize = 3;

/// The predicate operator an authored stub's fields are written under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Equals,
    DeepEquals,
    Contains,
    StartsWith,
    Matches,
    Exists,
    /// `not` over `equals`.
    Not,
}

/// A `copy` behavior's source: the request path, or the `q` query parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFrom {
    Path,
    Query,
}

/// The one request field a proxy's `predicateGenerators` records. Single-key on purpose: see
/// [`EXCLUDED`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generator {
    Path,
    Body,
}

/// The Mountebank-only decoration of a stub, drawn independently of the model's spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoration {
    pub operator: Operator,
    /// `repeat` on the first response, 1 to 3 (1 is written as no `repeat`).
    pub repeat: u8,
    pub copy: Option<CopyFrom>,
    /// A `wait` of 0 after any copy.
    pub wait: bool,
}

/// A stub a generated case authors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MbSpec {
    Is {
        pred: Pred,
        label: &'static str,
        responses: usize,
        decoration: Decoration,
    },
    Proxy {
        pred: Pred,
        operator: Operator,
        mode: Mode,
        generator: Generator,
    },
}

/// The model's spec without its Rift-only fields, plus `decoration`. The destructures name every
/// field and use no `..`: a field added to `ops::Spec` is a compile error here until it is either
/// mapped or listed in [`EXCLUDED`].
#[must_use]
pub fn mb_spec(spec: Spec, decoration: Decoration) -> MbSpec {
    match spec {
        Spec::Is {
            pred,
            gate: _,
            label,
            responses,
            conditional: _,
        } => MbSpec::Is {
            pred,
            label,
            responses,
            decoration,
        },
        Spec::Proxy {
            pred,
            mode,
            gen_body,
        } => MbSpec::Proxy {
            pred,
            operator: decoration.operator,
            mode,
            generator: if gen_body {
                Generator::Body
            } else {
                Generator::Path
            },
        },
    }
}

/// One edit of the authored stub list, as `ops::Edit` but over [`MbSpec`] (an `ops::Edit::Insert`
/// carries the model's spec, which has no room for the decoration).
#[derive(Clone, Debug)]
pub enum MbEdit {
    Insert(usize, MbSpec),
    Remove(usize),
    Swap(usize, usize),
    Clear,
}

/// Applies `edits` to `list`, every index taken modulo the length it applies to, so any
/// subsequence of a case is still a case.
pub fn apply_edits(list: &mut Vec<MbSpec>, edits: &[MbEdit]) {
    for edit in edits {
        match edit {
            MbEdit::Insert(i, spec) => {
                let at = i % (list.len() + 1);
                list.insert(at, spec.clone());
            }
            MbEdit::Remove(i) if !list.is_empty() => {
                let at = i % list.len();
                list.remove(at);
            }
            MbEdit::Swap(i, j) if list.len() >= 2 => {
                let n = list.len();
                list.swap(i % n, j % n);
            }
            MbEdit::Clear => list.clear(),
            MbEdit::Remove(_) | MbEdit::Swap(..) => {}
        }
    }
}

/// An operation Mountebank has a route for. Indices are taken modulo `len + 1` of the authored
/// list when emitted (see `emit`), so one past the end is reachable: both engines answer it.
#[derive(Clone, Debug)]
pub enum MbOp {
    Request(Shape),
    /// `POST /imposters/:port/stubs` with `{index, stub}`.
    AddStub {
        index: usize,
        spec: MbSpec,
    },
    /// `DELETE /imposters/:port/stubs/:index`.
    DeleteStub(usize),
    /// `PUT /imposters/:port/stubs/:index`.
    ReplaceStub {
        index: usize,
        spec: MbSpec,
    },
    /// `PUT /imposters/:port/stubs` with the authored stubs, edited.
    ReplaceStubs(Vec<MbEdit>),
    /// `PUT /imposters` with both imposters, the authored stubs edited, in either body order.
    PutImposters {
        edits: Vec<MbEdit>,
        upstream_first: bool,
    },
    /// `DELETE /imposters/:port`, then `POST /imposters` with the authored stubs, edited.
    Recreate(Vec<MbEdit>),
    /// `DELETE /imposters/:port/savedProxyResponses`.
    ClearProxies,
    /// `DELETE /imposters/:port/savedRequests`.
    ClearRequests,
}

/// Why something the model test generates is not generated here: an allow-list entry, a doc line
/// that must still say what is quoted, or a plain choice that needs no excuse to keep.
#[derive(Clone, Copy, Debug)]
pub enum Excuse {
    /// An `allowlist.json` id: the difference is documented, but pinned to a hand-written case's
    /// locations, which a generated case cannot reuse.
    AllowList(&'static str),
    /// A doc file and a verbatim quote from it (whitespace-insensitive).
    Doc {
        file: &'static str,
        quote: &'static str,
    },
    /// Not a difference: the shape only costs time, or Mountebank has no counterpart to compare.
    Choice(&'static str),
}

/// Every vocabulary exclusion with its excuse. A test checks each cited allow-list id and doc
/// quote still exists, so an exclusion goes away together with its excuse.
pub const EXCLUDED: &[(&str, Excuse)] = &[
    (
        "predicateGenerators.matches with more than one key",
        Excuse::AllowList(
            "generator-shape/predicategenerators-with-metho/stored[4545]/stubs/0/predicates#0",
        ),
    ),
    (
        "inject / decorate responses and predicates",
        Excuse::Doc {
            file: "docs/mountebank/differences.md",
            quote: "Mountebank evaluates `inject`, `decorate` and `wait` functions inside its own \
                    Node process",
        },
    ),
    (
        "shellTransform",
        Excuse::AllowList("shell-transform-failure-lenient#0"),
    ),
    (
        "scenarioName / requiredScenarioState / newScenarioState (ops::Gate)",
        Excuse::Doc {
            file: "docs/features/scenarios.md",
            quote: "Each stub can require a state to be eligible and can transition the state \
                    after it responds.",
        },
    ),
    (
        "_rift.conditional (ops::Spec::Is::conditional)",
        Excuse::Doc {
            file: "docs/features/index.md",
            quote: "`Last-Modified` and `304 Not Modified` on an `is` response with \
                    `_rift.conditional`",
        },
    ),
    (
        "SetScenarioState, ResetScenarios, FlowPut, FlowDelete, FlowClear",
        Excuse::Doc {
            file: "docs/features/flow-state.md",
            quote: "Flow state is a per-flow key/value store used to build stateful mocks",
        },
    ),
    (
        "Reload",
        Excuse::Doc {
            file: "docs/features/hot-reload.md",
            quote: "`POST /admin/reload` re-reads the startup config source",
        },
    ),
    (
        "MoveStub",
        Excuse::Choice("Mountebank 2.9.5 has no move route (src/mountebank.js)"),
    ),
    (
        "the X-Flow request header",
        Excuse::Choice("the model's flowIdSource; to Mountebank it is only noise"),
    ),
    (
        "a non-zero wait",
        Excuse::Choice("adds wall-clock time to every case and reaches no further code"),
    ),
];

fn operator() -> impl Strategy<Value = Operator> {
    prop_oneof![
        6 => Just(Operator::Equals),
        1 => Just(Operator::DeepEquals),
        1 => Just(Operator::Contains),
        1 => Just(Operator::StartsWith),
        1 => Just(Operator::Matches),
        1 => Just(Operator::Exists),
        1 => Just(Operator::Not),
    ]
}

fn decoration() -> impl Strategy<Value = Decoration> {
    (
        operator(),
        prop_oneof![4 => Just(1u8), 1 => Just(2u8), 1 => Just(3u8)],
        prop::option::weighted(
            0.2,
            prop_oneof![Just(CopyFrom::Path), Just(CopyFrom::Query)],
        ),
        prop::bool::weighted(0.1),
    )
        .prop_map(|(operator, repeat, copy, wait)| Decoration {
            operator,
            repeat,
            copy,
            wait,
        })
}

pub fn spec() -> impl Strategy<Value = MbSpec> {
    (ops::spec(), decoration()).prop_map(|(spec, decoration)| mb_spec(spec, decoration))
}

fn edits() -> impl Strategy<Value = Vec<MbEdit>> {
    let edit = prop_oneof![
        3 => (0..8usize, spec()).prop_map(|(i, s)| MbEdit::Insert(i, s)),
        3 => (0..8usize).prop_map(MbEdit::Remove),
        2 => (0..8usize, 0..8usize).prop_map(|(i, j)| MbEdit::Swap(i, j)),
        1 => Just(MbEdit::Clear),
    ];
    prop::collection::vec(edit, 0..4)
}

pub fn op() -> impl Strategy<Value = MbOp> {
    prop_oneof![
        12 => ops::shape().prop_map(MbOp::Request),
        3 => (0..8usize, spec()).prop_map(|(index, spec)| MbOp::AddStub { index, spec }),
        1 => (0..8usize).prop_map(MbOp::DeleteStub),
        1 => (0..8usize, spec()).prop_map(|(index, spec)| MbOp::ReplaceStub { index, spec }),
        1 => edits().prop_map(MbOp::ReplaceStubs),
        1 => (edits(), any::<bool>())
            .prop_map(|(edits, upstream_first)| MbOp::PutImposters { edits, upstream_first }),
        1 => edits().prop_map(MbOp::Recreate),
        1 => Just(MbOp::ClearProxies),
        1 => Just(MbOp::ClearRequests),
    ]
}

/// A case: up to 6 stubs the imposter starts with, then 1 to 39 operations (the model's bounds).
pub fn case() -> impl Strategy<Value = (Vec<MbSpec>, Vec<MbOp>)> {
    (
        prop::collection::vec(spec(), 0..6),
        prop::collection::vec(op(), 1..40),
    )
}

/// The upstream imposter: per path, a stub answering `up<path>#0`, `#1`, `#2` in turn.
#[must_use]
pub fn upstream() -> Value {
    let stubs: Vec<Value> = ops::PATHS
        .iter()
        .map(|path| {
            let responses: Vec<Value> = (0..UPSTREAM_ANSWERS)
                .map(|k| json!({ "is": { "body": format!("up{path}#{k}") } }))
                .collect();
            json!({ "predicates": [{ "equals": { "path": path } }], "responses": responses })
        })
        .collect();
    json!({ "port": UPSTREAM, "protocol": "http", "stubs": stubs })
}

/// The imposter under test with `stubs`.
#[must_use]
pub fn imposter(stubs: &[MbSpec]) -> Value {
    let stubs: Vec<Value> = stubs.iter().map(MbSpec::to_json).collect();
    json!({ "port": IMPOSTER, "protocol": "http", "recordRequests": true, "stubs": stubs })
}

/// The predicate fields that are set, each written as `value` returns it.
fn fields(pred: &Pred, value: impl Fn(&str) -> Value) -> Map<String, Value> {
    let Pred {
        method,
        path,
        query,
        body,
    } = pred;
    let mut out = Map::new();
    if let Some(m) = method {
        out.insert("method".into(), value(m.as_str()));
    }
    if let Some(p) = path {
        out.insert("path".into(), value(p));
    }
    if *query {
        out.insert("query".into(), json!({ "q": value("1") }));
    }
    if let Some(b) = body {
        out.insert("body".into(), value(b));
    }
    out
}

fn predicates(pred: &Pred, operator: Operator) -> Value {
    let literal = |v: &str| json!(v);
    let set = fields(pred, literal);
    if set.is_empty() {
        return json!([]);
    }
    let predicate = match operator {
        Operator::Equals => json!({ "equals": set }),
        Operator::DeepEquals => json!({ "deepEquals": set }),
        Operator::Contains => json!({ "contains": set }),
        Operator::StartsWith => json!({ "startsWith": set }),
        Operator::Matches => {
            json!({ "matches": fields(pred, |v| json!(format!("^{}$", escape_regex(v)))) })
        }
        Operator::Exists => json!({ "exists": fields(pred, |_| json!(true)) }),
        Operator::Not => json!({ "not": { "equals": set } }),
    };
    json!([predicate])
}

/// The few regex metacharacters the closed value sets can hold (`/` needs none).
fn escape_regex(value: &str) -> String {
    value.chars().fold(String::new(), |mut out, c| {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

impl MbSpec {
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            MbSpec::Is {
                pred,
                label,
                responses,
                decoration,
            } => {
                let Decoration {
                    operator,
                    repeat,
                    copy,
                    wait,
                } = decoration;
                let mut behaviors = Vec::new();
                if let Some(from) = copy {
                    let from = match from {
                        CopyFrom::Path => json!("path"),
                        CopyFrom::Query => json!({ "query": "q" }),
                    };
                    behaviors.push(json!({ "copy": {
                        "from": from,
                        "into": "${C}",
                        "using": { "method": "regex", "selector": ".*" }
                    }}));
                }
                if *wait {
                    behaviors.push(json!({ "wait": 0 }));
                }
                let responses: Vec<Value> = (0..*responses)
                    .map(|i| {
                        let (status, body) = ops::is_response(label, i);
                        let body = if copy.is_some() {
                            format!("{body}:${{C}}")
                        } else {
                            body
                        };
                        let mut response = json!({ "is": { "statusCode": status, "body": body } });
                        if i == 0 && *repeat > 1 {
                            response["repeat"] = json!(repeat);
                        }
                        if !behaviors.is_empty() {
                            response["behaviors"] = json!(behaviors);
                        }
                        response
                    })
                    .collect();
                json!({ "predicates": predicates(pred, *operator), "responses": responses })
            }
            MbSpec::Proxy {
                pred,
                operator,
                mode,
                generator,
            } => {
                let key = match generator {
                    Generator::Path => "path",
                    Generator::Body => "body",
                };
                json!({
                    "predicates": predicates(pred, *operator),
                    "responses": [{ "proxy": {
                        "to": format!("http://localhost:{UPSTREAM}"),
                        "mode": mode.as_str(),
                        "predicateGenerators": [{ "matches": { key: true } }]
                    }}]
                })
            }
        }
    }
}

/// The `send` step for one request shape, without the model's `X-Flow` header.
#[must_use]
pub fn request(shape: &Shape) -> Value {
    let Shape {
        method,
        path,
        query,
        body,
        flow: _,
    } = shape;
    let path = if *query {
        format!("{path}?q=1")
    } else {
        (*path).to_string()
    };
    let mut step =
        json!({ "do": "send", "port": IMPOSTER, "method": method.as_str(), "path": path });
    if let (Method::Post, Some(body)) = (method, body) {
        step["body"] = json!(body);
    }
    step
}
