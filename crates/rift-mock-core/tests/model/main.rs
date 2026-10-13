//! Model-based state-machine test of the stateful core (issue #1344).
//!
//! A case is a random sequence of admin operations and requests (`ops.rs`) run against a real
//! [`ImposterManager`] — real listeners, a real upstream imposter behind the proxy stubs — and
//! against a small reference model (`model.rs`). After every operation the two are compared:
//!
//! - a request's status, body, and whether a proxy stub answered it (`x-rift-proxy`);
//! - the stub list, in order, recordings and their `recordedFrom` included;
//! - every scenario state and raw flow-state value of both flows;
//! - the upstream's request count;
//! - `Last-Modified` (#1280, #1301, #1303, #1307): for one request shape, a different answer must
//!   never be served under a `Last-Modified` at or before the previous answer's, or a client
//!   revalidating with `If-Modified-Since` is told `304` and keeps the stale one. The relation is
//!   checked on what is served, not by re-deriving the load-clock rules, so it holds whichever
//!   stamping scheme produces it. The load clock is wall-clock seconds with a floor that each stamp
//!   steps past, so it needs no injected time.
//!
//! Out of scope: intercept, TLS, templating, scripts. `PROPTEST_CASES` sets the case count (64 by
//! default, as in CI; the nightly `model-tests` workflow runs 2048). A failing case is shrunk and
//! its seed persisted in `tests/proptest-regressions/model.txt`, which is replayed first on every
//! later run.

mod model;
mod ops;

use std::collections::HashMap;
use std::sync::LazyLock;

use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, TestCaseError, TestRunner};
use rift_mock_core::imposter::{ImposterConfig, ImposterError, ImposterManager, Stub};
use serde_json::{Value, json};

use model::{Model, Reload, Served, UPSTREAM_RESPONSES, upstream_body};
use ops::{Edit, FLOW_KEYS, FLOWS, Method, Op, PATHS, SCENARIOS, Shape, Spec, apply_edits};

/// No connection pool: ports are reused (auto-assignment hands out the lowest free one, and
/// `Recreate` re-binds the same one), and a pooled connection to a deleted imposter is closed by the
/// time the next request is written on it.
static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("http client")
});

/// The engine side of a case: the imposter under test and the upstream its proxies forward to.
struct Sut {
    manager: ImposterManager,
    port: u16,
    upstream_port: u16,
    upstream: String,
}

fn imposter_json(port: u16, stubs: Value) -> Value {
    json!({
        "port": port,
        "protocol": "http",
        "_rift": { "flowState": { "flowIdSource": "header:X-Flow" } },
        "stubs": stubs
    })
}

fn config(value: Value) -> ImposterConfig {
    serde_json::from_value(value).expect("imposter config")
}

fn stub(value: Value) -> Stub {
    serde_json::from_value(value).expect("stub")
}

/// What the stub list shows for a stub, in the terms of [`model::Kind::fingerprint`]: a proxy's
/// mode, else its response bodies, scenario fields, and `recordedFrom`.
fn fingerprint(stub: &Stub) -> String {
    let v = serde_json::to_value(stub).expect("stub serializes");
    let responses = v["responses"].as_array().expect("responses").clone();
    if let Some(proxy) = responses.iter().find_map(|r| r.get("proxy")) {
        let mode = proxy["mode"].as_str().expect("a proxy mode");
        return format!("proxy:{mode} {}", v["predicates"]);
    }
    let bodies: Vec<String> = responses
        .iter()
        .map(|r| {
            r["is"]["body"]
                .as_str()
                .expect("an is response with a string body")
                .to_string()
        })
        .collect();
    if let Some(from) = v["recordedFrom"].as_str() {
        return format!("rec@{from}[{}]", bodies.join(","));
    }
    let gate = match v["scenarioName"].as_str() {
        Some(scenario) => format!(
            " {scenario}:{}->{}",
            v["requiredScenarioState"].as_str().unwrap_or("*"),
            v["newScenarioState"].as_str().unwrap_or("=")
        ),
        None => String::new(),
    };
    format!("is[{}]{gate} {}", bodies.join(","), v["predicates"])
}

impl Sut {
    async fn start(specs: &[Spec]) -> Self {
        let manager = ImposterManager::new();
        let upstream_stubs: Vec<Value> = PATHS
            .iter()
            .map(|path| {
                let responses: Vec<Value> = (0..UPSTREAM_RESPONSES)
                    .map(|hit| json!({ "is": { "statusCode": 200, "body": upstream_body(path, hit) } }))
                    .collect();
                json!({ "predicates": [{ "equals": { "path": path } }], "responses": responses })
            })
            .collect();
        let upstream_port = manager
            .create_imposter(config(json!({
                "port": 0, "protocol": "http", "recordRequests": true, "stubs": upstream_stubs
            })))
            .await
            .expect("upstream imposter");
        let upstream = format!("http://127.0.0.1:{upstream_port}");
        let stubs: Vec<Value> = specs.iter().map(|s| s.to_json(&upstream)).collect();
        let port = manager
            .create_imposter(config(imposter_json(0, json!(stubs))))
            .await
            .expect("imposter under test");
        Sut {
            manager,
            port,
            upstream_port,
            upstream,
        }
    }

    fn imposter(&self) -> std::sync::Arc<rift_mock_core::imposter::Imposter> {
        self.manager
            .get_imposter(self.port)
            .expect("imposter exists")
    }

    fn edited(&self, edits: &[Edit]) -> Vec<Stub> {
        let mut stubs = self.imposter().get_stubs();
        apply_edits(&mut stubs, edits, &|spec: &Spec| {
            stub(spec.to_json(&self.upstream))
        });
        stubs
    }

    fn config_with(&self, stubs: Vec<Stub>) -> ImposterConfig {
        let mut config = config(imposter_json(self.port, json!([])));
        config.stubs = stubs;
        config
    }

    async fn request(&self, r: &Shape) -> (Served, Option<i64>) {
        let query = if r.query { "?q=1" } else { "" };
        let url = format!("http://127.0.0.1:{}{}{query}", self.port, r.path);
        let mut request = match r.method {
            Method::Get => CLIENT.get(url),
            Method::Post => CLIENT.post(url),
        }
        .header("X-Flow", r.flow);
        if let Some(body) = r.body {
            request = request.body(body);
        }
        let response = request.send().await.expect("request reaches the imposter");
        let status = response.status().as_u16();
        let proxied = response.headers().contains_key("x-rift-proxy");
        let last_modified = response.headers().get("last-modified").map(|v| {
            let text = v.to_str().expect("Last-Modified is ASCII");
            chrono::DateTime::parse_from_rfc2822(text)
                .unwrap_or_else(|e| panic!("Last-Modified {text:?} is an HTTP date: {e}"))
                .timestamp()
        });
        let body = response.text().await.expect("response body");
        let served = Served {
            status,
            body,
            proxied,
            last_modified: last_modified.is_some(),
        };
        (served, last_modified)
    }
}

/// For each request shape, the last answer it got that carried `Last-Modified`.
#[derive(Default)]
struct Revalidation(HashMap<Shape, (u16, String, i64)>);

impl Revalidation {
    fn observe(&mut self, r: &Shape, served: &Served, lm: Option<i64>) -> Result<(), String> {
        let Some(lm) = lm else {
            // The client now holds an answer without a validator: nothing to revalidate against.
            self.0.remove(r);
            return Ok(());
        };
        if let Some((status, body, prev)) = self.0.get(r)
            && (*status != served.status || *body != served.body)
            && lm <= *prev
        {
            return Err(format!(
                "{r:?} changed from {status} {body:?} to {} {:?} without Last-Modified moving past \
                 {prev} (now {lm}): a client revalidating gets 304 and keeps the stale answer",
                served.status, served.body
            ));
        }
        self.0
            .insert(r.clone(), (served.status, served.body.clone(), lm));
        Ok(())
    }
}

async fn apply(
    sut: &mut Sut,
    model: &mut Model,
    op: &Op,
    lm: &mut Revalidation,
) -> Result<(), TestCaseError> {
    match op {
        Op::Request(r) => {
            let expected = model.request(r);
            let (served, stamp) = sut.request(r).await;
            prop_assert_eq!(&served, &expected, "request {:?}", r);
            lm.observe(r, &served, stamp).map_err(TestCaseError::fail)?;
        }
        Op::ReplaceStubs(edits) => {
            let stubs = sut.edited(edits);
            model.replace_stubs(edits);
            sut.manager
                .replace_stubs(sut.port, stubs)
                .await
                .expect("replace stubs");
        }
        Op::AddStub { index, spec } => {
            let at = model.add_stub(*index, spec);
            sut.manager
                .add_stub(sut.port, stub(spec.to_json(&sut.upstream)), Some(at))
                .await
                .expect("add stub");
        }
        Op::DeleteStub(index) => {
            let at = model.delete_stub(*index);
            let result = sut.manager.delete_stub(sut.port, at.unwrap_or(0)).await;
            prop_assert_eq!(
                result.is_ok(),
                at.is_some(),
                "delete stub {:?}: {:?}",
                at,
                result
            );
        }
        Op::MoveStub(from, to) => {
            let at = model.move_stub(*from, *to);
            let (from, to) = at.unwrap_or((0, 0));
            let result = sut.manager.move_stub(sut.port, from, to).await;
            prop_assert_eq!(
                result.is_ok(),
                at.is_some(),
                "move stub {:?}: {:?}",
                at,
                result
            );
        }
        Op::SetScenarioState {
            flow,
            scenario,
            state,
        } => {
            model.set_scenario_state(flow, scenario, state);
            sut.imposter()
                .set_scenario_state(flow, scenario, state)
                .expect("set scenario state");
        }
        Op::ResetScenarios { flow } => {
            // What `POST /imposters/:port/scenarios/reset` does.
            model.reset_scenarios(flow);
            let imposter = sut.imposter();
            for name in imposter.scenario_names() {
                imposter
                    .delete_scenario_state(flow, &name)
                    .expect("reset scenario");
            }
        }
        Op::FlowPut { flow, key, value } => {
            model.flow_put(flow, key, value.clone());
            sut.imposter()
                .flow_set(flow, key, value.clone())
                .expect("flow put");
        }
        Op::FlowDelete { flow, key } => {
            model.flow_delete(flow, key);
            sut.imposter().flow_delete(flow, key).expect("flow delete");
        }
        Op::FlowClear { flow } => {
            model.flow_clear(flow);
            sut.imposter().flow_clear(flow).expect("flow clear");
        }
        Op::Reload(edits) => {
            let config = sut.config_with(sut.edited(edits));
            let expected = model.reload(edits);
            let report = sut
                .manager
                .apply_one(config)
                .await
                .expect("reload validates");
            prop_assert!(
                report.failed.is_empty(),
                "reload failed: {:?}",
                report.failed
            );
            let outcome = if report.replaced.contains(&sut.port) {
                Reload::Replaced
            } else if report.stub_patched.contains(&sut.port) {
                Reload::Patched
            } else {
                Reload::Unchanged
            };
            prop_assert_eq!(outcome, expected, "reload outcome: {:?}", report);
        }
        Op::Recreate(edits) => {
            let config = sut.config_with(sut.edited(edits));
            model.recreate(edits);
            sut.manager
                .delete_imposter(sut.port)
                .await
                .expect("delete imposter");
            // The port was free a moment ago, but the manager auto-assigns ports to every other
            // test in this binary, so one may have taken it. Then the imposter moves to a new port,
            // which a client sees as a new URL holding nothing cached.
            let port = match sut.manager.create_imposter(config.clone()).await {
                Err(ImposterError::BindError(..)) => {
                    lm.0.clear();
                    let mut moved = config;
                    moved.port = None;
                    sut.manager
                        .create_imposter(moved)
                        .await
                        .expect("re-create imposter on a new port")
                }
                created => created.expect("re-create imposter"),
            };
            sut.port = port;
        }
    }
    compare_state(sut, model)
}

fn compare_state(sut: &Sut, model: &Model) -> Result<(), TestCaseError> {
    let imposter = sut.imposter();
    let stubs: Vec<String> = imposter.get_stubs().iter().map(fingerprint).collect();
    prop_assert_eq!(stubs, model.fingerprints(&sut.upstream), "stub list");
    for flow in FLOWS {
        for key in FLOW_KEYS.iter().chain(SCENARIOS.iter()) {
            let actual = imposter.flow_get(flow, key).expect("flow read");
            prop_assert_eq!(
                actual.as_ref(),
                model.kv.get(&(flow, *key)),
                "flow state {}/{}",
                flow,
                key
            );
        }
    }
    let hits = sut
        .manager
        .get_imposter(sut.upstream_port)
        .expect("upstream exists")
        .get_recorded_requests()
        .len();
    prop_assert_eq!(hits, model.upstream_hits(), "upstream hits");
    Ok(())
}

async fn run_case(specs: &[Spec], ops: &[Op]) -> Result<(), TestCaseError> {
    let mut sut = Sut::start(specs).await;
    let mut model = Model::new(specs);
    let mut lm = Revalidation::default();
    let mut outcome = compare_state(&sut, &model);
    for (step, op) in ops.iter().enumerate() {
        if outcome.is_err() {
            break;
        }
        outcome = apply(&mut sut, &mut model, op, &mut lm)
            .await
            .map_err(|e| TestCaseError::fail(format!("step {step} {op:?}: {e}")));
    }
    sut.manager.delete_all().await;
    outcome
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// `PROPTEST_CASES`, or 64. A value that does not parse fails the run rather than quietly running
/// the default: the nightly job would otherwise pass green on a fraction of its cases.
fn cases() -> u32 {
    match std::env::var("PROPTEST_CASES") {
        Ok(v) => v
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("PROPTEST_CASES={v:?}: {e}")),
        Err(std::env::VarError::NotPresent) => 64,
        Err(e) => panic!("PROPTEST_CASES: {e}"),
    }
}

#[test]
fn engine_matches_the_model() {
    let rt = runtime();
    let mut runner = TestRunner::new(Config {
        cases: cases(),
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/proptest-regressions/model.txt"
        )))),
        ..Config::default()
    });
    let result = runner.run(&ops::case(), |(specs, ops)| {
        rt.block_on(run_case(&specs, &ops))
    });
    result.unwrap_or_else(|e| panic!("{e}"));
}

fn run(specs: &[Spec], ops: &[Op]) {
    runtime()
        .block_on(run_case(specs, ops))
        .unwrap_or_else(|e| panic!("{e}"));
}

fn conditional(path: &'static str, label: &'static str) -> Spec {
    Spec::Is {
        pred: ops::Pred {
            path: Some(path),
            ..ops::Pred::default()
        },
        gate: None,
        label,
        responses: 1,
        conditional: true,
    }
}

fn get(path: &'static str) -> Op {
    Op::Request(Shape {
        method: Method::Get,
        path,
        query: false,
        body: None,
        flow: "f1",
    })
}

/// #1303: deleting the stub that answered exposes an older stub behind it; its answer must come
/// with a `Last-Modified` past the deleted stub's, or a revalidating client is told `304`.
#[test]
fn deleting_the_answering_stub_moves_last_modified_forward() {
    run(
        &[conditional("/a", "p")],
        &[
            Op::AddStub {
                index: 0,
                spec: conditional("/a", "q"),
            },
            get("/a"),
            Op::DeleteStub(0),
            get("/a"),
        ],
    );
}

/// A conditional stub gated on `S1` being `A`, ahead of an ungated conditional stub on the same
/// path: which one answers is decided by the scenario alone.
fn gated_ahead_of_ungated() -> Vec<Spec> {
    let gated = Spec::Is {
        pred: ops::Pred {
            path: Some("/a"),
            ..ops::Pred::default()
        },
        gate: Some(ops::Gate {
            scenario: "S1",
            required: Some("A"),
            next: None,
        }),
        label: "q",
        responses: 1,
        conditional: true,
    };
    vec![gated, conditional("/a", "p")]
}

fn set_s1(state: &'static str) -> Op {
    Op::SetScenarioState {
        flow: "f1",
        scenario: "S1",
        state,
    }
}

/// #1307 through the flow-state door: clearing a flow returns its scenarios to their initial
/// state, which changes the answer, so it must move `Last-Modified` like any other transition.
#[test]
fn clearing_a_flow_moves_last_modified_forward() {
    run(
        &gated_ahead_of_ungated(),
        &[
            get("/a"),
            set_s1("A"),
            get("/a"),
            Op::FlowClear { flow: "f1" },
            get("/a"),
        ],
    );
}

/// Writing a scenario's key as raw flow state is a transition.
#[test]
fn putting_a_scenario_key_moves_last_modified_forward() {
    run(
        &gated_ahead_of_ungated(),
        &[
            get("/a"),
            Op::FlowPut {
                flow: "f1",
                key: "S1",
                value: json!("A"),
            },
            get("/a"),
        ],
    );
}

/// Deleting a scenario's key returns it to `Started`, which is a transition.
#[test]
fn deleting_a_scenario_key_moves_last_modified_forward() {
    run(
        &gated_ahead_of_ungated(),
        &[
            get("/a"),
            set_s1("A"),
            get("/a"),
            Op::FlowDelete {
                flow: "f1",
                key: "S1",
            },
            get("/a"),
        ],
    );
}

/// #1333: a `proxyOnce` request whose generated predicates miss every recording is forwarded and
/// recorded, not answered from another identity's recording.
#[test]
fn a_generator_miss_is_forwarded_and_recorded() {
    let post = |body| {
        Op::Request(Shape {
            method: Method::Post,
            path: "/a",
            query: false,
            body: Some(body),
            flow: "f1",
        })
    };
    run(
        &[Spec::Proxy {
            pred: ops::Pred::default(),
            mode: ops::Mode::Once,
            gen_body: true,
        }],
        &[post("x"), post("y"), post("x"), post("y")],
    );
}

/// The other side of the flow-state stamping: a raw write that leaves the scenario where it was is
/// not a transition, so the answer keeps its `Last-Modified` (and a poller keeps its `304`).
#[test]
fn flow_writes_that_change_nothing_keep_last_modified() {
    runtime().block_on(async {
        let sut = Sut::start(&gated_ahead_of_ungated()).await;
        let imposter = sut.imposter();
        let shape = Shape {
            method: Method::Get,
            path: "/a",
            query: false,
            body: None,
            flow: "f1",
        };
        imposter
            .set_scenario_state("f1", "S1", "A")
            .expect("set scenario state");
        let (served, before) = sut.request(&shape).await;
        assert_eq!(served.body, "q0");

        imposter
            .flow_set("f1", "S1", json!("A"))
            .expect("same state");
        imposter.flow_delete("f2", "S1").expect("absent key");
        imposter.flow_clear("f2").expect("empty flow");
        imposter
            .flow_set("f1", "k1", json!(1))
            .expect("not a scenario");

        let (served, after) = sut.request(&shape).await;
        assert_eq!(served.body, "q0");
        assert_eq!(after, before, "Last-Modified moved without a transition");
        sut.manager.delete_all().await;
    });
}

/// #1387: a `proxyOnce` stub added to an imposter that was created without a proxy replays its
/// recording; it is not left ungated by the mode the imposter was built with.
#[test]
fn a_proxy_once_stub_added_later_gates_by_its_own_mode() {
    let post = |path| {
        Op::Request(Shape {
            method: Method::Post,
            path,
            query: false,
            body: Some("x"),
            flow: "f1",
        })
    };
    run(
        &[conditional("/b", "p")],
        &[
            Op::AddStub {
                index: 1,
                spec: Spec::Proxy {
                    pred: ops::Pred::default(),
                    mode: ops::Mode::Once,
                    gen_body: true,
                },
            },
            post("/a"),
            // Move the recording behind the proxy (moving keeps its claim, #1386): the next
            // request reaches the proxy, which must replay rather than forward.
            Op::MoveStub(1, 2),
            post("/a"),
        ],
    );
}

/// #1387: a `proxyAlways` stub behind a `proxyOnce` one forwards every request that reaches it,
/// rather than replaying what the imposter-wide `proxyOnce` store recorded for it. The recording
/// is deleted so the next request reaches the proxy again (it lands at index 1, #1389).
#[test]
fn a_proxy_always_stub_behind_a_proxy_once_stub_always_forwards() {
    let proxy = |path, mode| Spec::Proxy {
        pred: ops::Pred {
            path: Some(path),
            ..ops::Pred::default()
        },
        mode,
        gen_body: false,
    };
    run(
        &[proxy("/a", ops::Mode::Once), proxy("/b", ops::Mode::Always)],
        &[get("/b"), Op::DeleteStub(1), get("/b")],
    );
}

/// #1389: a `proxyOnce` recording goes in front of the proxy stub that answered, not the first
/// proxy stub to the same upstream, so the `GET` stub between the two proxies still answers.
#[test]
fn a_proxy_once_recording_is_placed_by_the_proxy_that_answered() {
    let proxy = |path| Spec::Proxy {
        pred: ops::Pred {
            path: Some(path),
            ..ops::Pred::default()
        },
        mode: ops::Mode::Once,
        gen_body: false,
    };
    let get_a = Spec::Is {
        pred: ops::Pred {
            method: Some(Method::Get),
            path: Some("/a"),
            ..ops::Pred::default()
        },
        gate: None,
        label: "p",
        responses: 1,
        conditional: false,
    };
    run(
        &[proxy("/b"), get_a, proxy("/a")],
        &[
            Op::Request(Shape {
                method: Method::Post,
                path: "/a",
                query: false,
                body: None,
                flow: "f1",
            }),
            get("/a"),
        ],
    );
}

/// #1389: a `proxyAlways` recording with no equal-predicate stub after the proxy goes at the end
/// of the list, behind every stub authored after the proxy.
#[test]
fn a_proxy_always_recording_is_appended_at_the_end() {
    let catch_all = Spec::Proxy {
        pred: ops::Pred::default(),
        mode: ops::Mode::Always,
        gen_body: false,
    };
    run(
        &[catch_all, conditional("/b", "p")],
        &[get("/a"), get("/a")],
    );
}

/// #1386: deleting a `proxyOnce` recording forgets its claim, so the next request for that
/// identity is forwarded and recorded again, as in Mountebank, where the stub is the recording.
#[test]
fn deleting_a_proxy_once_recording_records_the_identity_again() {
    let post = Op::Request(Shape {
        method: Method::Post,
        path: "/a",
        query: false,
        body: None,
        flow: "f1",
    });
    run(
        &[Spec::Proxy {
            pred: ops::Pred::default(),
            mode: ops::Mode::Once,
            gen_body: false,
        }],
        &[post.clone(), Op::DeleteStub(0), post.clone(), post],
    );
}

/// #1386: `PUT /imposters/:port/stubs` without the recording forgets its claim too.
#[test]
fn replacing_the_stubs_without_a_recording_records_the_identity_again() {
    let post = Op::Request(Shape {
        method: Method::Post,
        path: "/a",
        query: false,
        body: None,
        flow: "f1",
    });
    run(
        &[Spec::Proxy {
            pred: ops::Pred::default(),
            mode: ops::Mode::Once,
            gen_body: false,
        }],
        &[
            post.clone(),
            Op::ReplaceStubs(vec![ops::Edit::Remove(0)]),
            post,
        ],
    );
}

fn post_a() -> Op {
    Op::Request(Shape {
        method: Method::Post,
        path: "/a",
        query: false,
        body: None,
        flow: "f1",
    })
}

fn once_catch_all() -> Spec {
    Spec::Proxy {
        pred: ops::Pred::default(),
        mode: ops::Mode::Once,
        gen_body: false,
    }
}

/// #1386: a reload that patches the recording away (a stub-level reconcile, not a replacement of
/// the imposter) forgets its claim.
#[test]
fn a_reload_that_drops_a_recording_records_the_identity_again() {
    run(
        &[once_catch_all()],
        &[post_a(), Op::Reload(vec![ops::Edit::Remove(0)]), post_a()],
    );
}

/// #1386: a reload that only moves the recording keeps its state, and with it the claim: moved
/// behind the proxy, the identity is replayed, not forwarded.
#[test]
fn a_reload_that_moves_a_recording_keeps_its_claim() {
    run(
        &[once_catch_all()],
        &[post_a(), Op::Reload(vec![ops::Edit::Swap(0, 1)]), post_a()],
    );
}
