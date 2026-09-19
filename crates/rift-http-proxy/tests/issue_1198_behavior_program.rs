//! Issue #1198: Mountebank's `behaviors` array is an ordered program — every element runs, in
//! array order (`behaviors.js` `execute`). Rift folded the array into one keyed block, so a
//! repeated `decorate` or `wait` ran once, element order was ignored, and the fold decided what the
//! `--allowInjection` gate and the parser saw.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Admin {
    client: reqwest::Client,
    url: String,
    manager: Arc<ImposterManager>,
}

impl Admin {
    async fn start(allow_injection: bool) -> Self {
        let manager = Arc::new(ImposterManager::new());
        let admin_port = free_port();
        let server = AdminApiServer::new(
            format!("127.0.0.1:{admin_port}").parse().expect("addr"),
            manager.clone(),
            None,
        )
        .with_allow_injection(allow_injection);
        tokio::spawn(server.run());
        let url = format!("http://127.0.0.1:{admin_port}");
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client.get(format!("{url}/imposters")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Admin {
            client,
            url,
            manager,
        }
    }

    async fn post(&self, imposter: &Value) -> reqwest::Response {
        self.client
            .post(format!("{}/imposters", self.url))
            .json(imposter)
            .send()
            .await
            .expect("POST")
    }

    /// Create an imposter serving `body` with `behaviors`, and return its port.
    async fn serve(&self, body: &str, behaviors: Value, strict: bool) -> u16 {
        let port = free_port();
        let imposter = json!({"port": port, "protocol": "http", "strictBehaviors": strict,
            "stubs": [{"responses": [{"is": {"body": body}, "behaviors": behaviors}]}]});
        let response = self.post(&imposter).await;
        assert_eq!(response.status().as_u16(), 201, "{imposter}");
        port
    }

    async fn get(&self, port: u16) -> reqwest::Response {
        self.client
            .get(format!("http://127.0.0.1:{port}/p"))
            .send()
            .await
            .expect("GET")
    }

    async fn saved_behaviors(&self, port: u16) -> Value {
        let imposter: Value = self
            .client
            .get(format!("{}/imposters/{port}", self.url))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("json");
        imposter["stubs"][0]["responses"][0]["behaviors"].clone()
    }

    async fn stop(&self, port: u16) {
        let _ = self.manager.delete_imposter(port).await;
    }
}

fn append(s: &str) -> String {
    format!("function (request, response) {{ response.body = response.body + '{s}'; }}")
}

const REPLACE_T: &str =
    "function (request, response) { response.body = response.body.replace('${T}', 'DEC-WON'); }";
const THROW: &str = "function (request, response) { throw new Error('boom'); }";

fn copy_path_into(token: &str) -> Value {
    json!({"from": "path", "into": token, "using": {"method": "regex", "selector": ".+"}})
}

// ---- the finding's table, as Mountebank 2.9.1 serves it ---------------------------------------

#[tokio::test]
async fn every_decorate_element_runs_in_order() {
    let admin = Admin::start(true).await;
    let port = admin
        .serve(
            "x",
            json!([{"decorate": append("1")}, {"decorate": append("2")}]),
            false,
        )
        .await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "x12");
    admin.stop(port).await;
}

#[tokio::test]
async fn every_wait_element_runs() {
    let admin = Admin::start(true).await;
    let port = admin
        .serve("x", json!([{"wait": 300}, {"wait": 300}]), false)
        .await;
    let start = Instant::now();
    admin.get(port).await.text().await.expect("body");
    assert!(
        start.elapsed() >= Duration::from_millis(600),
        "served after {:?}",
        start.elapsed()
    );
    admin.stop(port).await;
}

/// The decorate element comes first, so it replaces the token before the copy could fill it.
#[tokio::test]
async fn elements_run_in_array_order_not_a_fixed_one() {
    let admin = Admin::start(true).await;
    let port = admin
        .serve(
            "${T}",
            json!([{"decorate": REPLACE_T}, {"copy": copy_path_into("${T}")}]),
            false,
        )
        .await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "DEC-WON");
    admin.stop(port).await;
}

// ---- null, failure and strict semantics survive the model change -------------------------------

#[tokio::test]
async fn a_later_null_removes_every_earlier_element_of_its_key() {
    let admin = Admin::start(true).await;
    let port = admin
        .serve("x", json!([{"wait": 500}, {"wait": null}]), false)
        .await;
    let start = Instant::now();
    admin.get(port).await.text().await.expect("body");
    assert!(
        start.elapsed() < Duration::from_millis(400),
        "the removed wait ran: {:?}",
        start.elapsed()
    );
    admin.stop(port).await;

    let port = admin
        .serve(
            "x",
            json!([{"decorate": append("1")}, {"decorate": null}, {"decorate": append("2")}]),
            false,
        )
        .await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "x2");
    admin.stop(port).await;
}

/// Lenient: the failed step is signalled and the next one still runs. Strict: the first failure
/// is the response.
#[tokio::test]
async fn a_failed_step_continues_leniently_and_stops_strictly() {
    let admin = Admin::start(true).await;
    let steps = json!([{"decorate": THROW}, {"decorate": append("2")}]);

    let port = admin.serve("x", steps.clone(), false).await;
    let response = admin.get(port).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["x-rift-decorate-error"], "true");
    assert_eq!(response.text().await.expect("body"), "x2");
    admin.stop(port).await;

    let port = admin.serve("x", steps, true).await;
    assert_eq!(admin.get(port).await.status().as_u16(), 500);
    admin.stop(port).await;
}

/// An element that sets several behaviors runs them in Rift's canonical order — the order written
/// is not recoverable (`preserve_order` is off) — and rift-lint says so (W018).
#[tokio::test]
async fn a_multi_key_element_runs_in_canonical_order_and_lints_w018() {
    let admin = Admin::start(true).await;
    let behaviors = json!([{"decorate": REPLACE_T, "copy": copy_path_into("${T}")}]);
    let port = admin.serve("${T}", behaviors.clone(), false).await;
    assert_eq!(admin.get(port).await.text().await.expect("body"), "/p");
    admin.stop(port).await;

    let w018 = |behaviors: Value| {
        let imposter = json!({"port": 4545, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "x"}, "behaviors": behaviors}
        ]}]});
        rift_lint::lint_json(
            &imposter.to_string(),
            "fixture.json",
            &rift_lint::LintOptions::default(),
        )
        .issues
        .into_iter()
        .filter(|i| i.code == "W018")
        .map(|i| i.location.unwrap_or_default())
        .collect::<Vec<_>>()
    };
    assert_eq!(
        w018(behaviors),
        vec!["stubs[0].responses[0].behaviors[0]".to_string()]
    );
    assert_eq!(
        w018(json!([{"decorate": REPLACE_T}, {"copy": copy_path_into("${T}")}])),
        Vec::<String>::new()
    );
    // `repeat` is not a step, so it does not make an element multi-step.
    assert_eq!(
        w018(json!([{"wait": 1, "repeat": 2}])),
        Vec::<String>::new()
    );
}

// ---- what the gate and the parser see -----------------------------------------------------------

/// The fold kept only the last `wait`, so a script `wait` followed by a numeric one passed the gate
/// and then was dropped. Every element runs now, so every element must be gated.
#[tokio::test]
async fn a_script_wait_followed_by_a_numeric_wait_needs_allow_injection() {
    let admin = Admin::start(false).await;
    let port = free_port();
    let imposter = |behaviors: Value| {
        json!({"port": port, "protocol": "http", "stubs": [{"responses": [
            {"is": {"body": "x"}, "behaviors": behaviors}
        ]}]})
    };
    let refused = admin
        .post(&imposter(
            json!([{"wait": "function () { return 1; }"}, {"wait": 5}]),
        ))
        .await;
    assert_eq!(refused.status().as_u16(), 400);

    // A script step removed by a later null never runs, so it needs no flag.
    let admitted = admin
        .post(&imposter(
            json!([{"decorate": append("1")}, {"decorate": null}]),
        ))
        .await;
    assert_eq!(admitted.status().as_u16(), 201);
    admin.stop(port).await;
}

/// Every element is live, so a malformed earlier element is no longer hidden by a later one.
#[tokio::test]
async fn a_malformed_earlier_element_is_refused() {
    let admin = Admin::start(true).await;
    let port = free_port();
    let response = admin
        .post(
            &json!({"port": port, "protocol": "http", "stubs": [{"responses": [
                {"is": {"body": "x"}, "behaviors": [{"wait": true}, {"wait": 5}]}
            ]}]}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 400);
    let text = response.text().await.expect("body");
    assert!(text.contains("`wait`"), "{text}");
    admin.stop(port).await;
}

// ---- what is written back ------------------------------------------------------------------------

#[tokio::test]
async fn the_program_is_written_back_in_its_order() {
    let admin = Admin::start(true).await;
    let (a, b) = (append("a"), append("b"));
    let port = admin
        .serve(
            "x",
            json!([{"decorate": a.clone()}, {"wait": 1}, {"decorate": b.clone()}]),
            false,
        )
        .await;
    assert_eq!(
        admin.saved_behaviors(port).await,
        json!([{"decorate": a}, {"wait": 1}, {"decorate": b}])
    );
    admin.stop(port).await;

    // Copies that are not adjacent stay separate elements; the saved file keeps their order.
    let (ca, cb) = (copy_path_into("${A}"), copy_path_into("${B}"));
    let port = admin
        .serve(
            "x",
            json!([{"copy": ca.clone()}, {"wait": 1}, {"copy": cb.clone()}]),
            false,
        )
        .await;
    assert_eq!(
        admin.saved_behaviors(port).await,
        json!([{"copy": ca}, {"wait": 1}, {"copy": cb}])
    );
    admin.stop(port).await;
}

/// The object form is written back in the order it runs (Mountebank's, since #1198 part B), with
/// its lists grouped as before.
#[tokio::test]
async fn an_object_form_block_is_written_back_in_its_run_order() {
    let admin = Admin::start(true).await;
    let (ca, cb) = (copy_path_into("${A}"), copy_path_into("${B}"));
    let d = append("d");
    let port = free_port();
    let imposter = json!({"port": port, "protocol": "http", "stubs": [{"responses": [
        {"is": {"body": "x"}, "_behaviors": {
            "decorate": d.clone(), "wait": 1, "copy": [ca.clone(), cb.clone()],
            "shellTransform": ["cat"], "repeat": 2
        }}
    ]}]});
    assert_eq!(admin.post(&imposter).await.status().as_u16(), 201);
    assert_eq!(
        admin.saved_behaviors(port).await,
        json!([{"wait": 1}, {"copy": [ca, cb]}, {"shellTransform": "cat"}, {"decorate": d}])
    );
    admin.stop(port).await;
}
