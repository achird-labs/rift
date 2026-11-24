//! Issue #1850: Mountebank 2.10.0's `chaos` behavior. In the `behaviors` array form it replaces a
//! share of responses with an error status (`errorRate`, `errorStatusCode`, default 500) and an
//! empty body, keeping the headers, and delays a share of them by up to `maxLatencyMs` (default
//! 1000) at `latencyRate`. The `_behaviors` object form ignores it, as Mountebank's
//! `compatibility.js` upcast does. Every expectation here was probed against Mountebank 2.10.0.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_http_proxy::admin_api::AdminApiServer;
use rift_http_proxy::config_loader::{ConfigSource, load_configs};
use rift_http_proxy::imposter::ImposterManager;
use serde_json::{Value, json};

struct Admin {
    client: reqwest::Client,
    base: String,
    manager: Arc<ImposterManager>,
    _running: rift_http_proxy::admin_api::RunningAdminApi,
}

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Admin {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let running =
            AdminApiServer::new("127.0.0.1:0".parse().expect("addr"), manager.clone(), None)
                .bind()
                .await
                .expect("admin API binds");
        Self {
            client: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("client"),
            base: format!("http://{}", running.local_addr()),
            manager,
            _running: running,
        }
    }

    async fn post_imposter(&self, imposter: &Value) -> (u16, String) {
        let reply = self
            .client
            .post(format!("{}/imposters", self.base))
            .json(imposter)
            .send()
            .await
            .expect("POST /imposters");
        let status = reply.status().as_u16();
        (status, reply.text().await.expect("body"))
    }

    async fn post(&self, response: Value) -> (u16, String) {
        self.post_imposter(&json!({"protocol": "http", "stubs": [{"responses": [response]}]}))
            .await
    }

    async fn refuses(&self, response: Value, message: &str) {
        let (status, body) = self.post(response.clone()).await;
        assert_eq!(status, 400, "{response} must be refused: {body}");
        let envelope: Value = serde_json::from_str(&body).expect("400 body is JSON");
        let said = envelope["errors"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(said.contains(message), "{response}: {body}");
    }

    async fn admits_imposter(&self, imposter: &Value) -> u16 {
        let (status, body) = self.post_imposter(imposter).await;
        assert_eq!(status, 201, "{imposter} must be admitted: {body}");
        let created: Value = serde_json::from_str(&body).expect("201 body is JSON");
        created["port"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .expect("assigned port")
    }

    async fn admits(&self, response: Value) -> u16 {
        self.admits_imposter(&json!({"protocol": "http", "stubs": [{"responses": [response]}]}))
            .await
    }

    async fn get(&self, port: u16, path: &str) -> Reply {
        let reply = self
            .client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .expect("GET");
        Reply {
            status: reply.status().as_u16(),
            headers: reply.headers().clone(),
            body: reply.text().await.expect("body"),
        }
    }

    async fn definition(&self, port: u16) -> Value {
        self.client
            .get(format!("{}/imposters/{port}", self.base))
            .send()
            .await
            .expect("GET /imposters/:port")
            .json()
            .await
            .expect("definition JSON")
    }

    async fn stop(&self, port: u16) {
        let _ = self.manager.delete_imposter(port).await;
    }
}

/// The stub every serve test answers with, before the behaviors run.
fn ok_with(behaviors: Value) -> Value {
    json!({
        "is": {"statusCode": 200, "headers": {"X-A": "1", "Content-Type": "text/plain"}, "body": "ok"},
        "behaviors": behaviors,
    })
}

const ERROR_RATE: &str = "chaos behavior \"errorRate\" field must be a number, representing \
                          probability between 0 and 1 of injecting an error response";
const ERROR_STATUS: &str = "chaos behavior \"errorStatusCode\" field must be an integer greater \
                            than or equal to 0";
const ERROR_STATUS_TYPE: &str = "chaos behavior \"errorStatusCode\" field must be a number, \
                                 representing the status code to return when injecting an error \
                                 (defaults to 500)";
const MAX_LATENCY_TYPE: &str = "chaos behavior \"maxLatencyMs\" field must be a number, \
                                representing maximum latency in milliseconds when injecting";
const LATENCY_RATE: &str = "chaos behavior \"latencyRate\" field must be a number, representing \
                            probability between 0 and 1 of injecting random latency";
const MAX_LATENCY: &str = "chaos behavior \"maxLatencyMs\" field must be an integer greater than \
                           or equal to 0";

// ---- config doors ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_chaos_field_of_the_wrong_type_is_refused_in_mountebanks_words() {
    let admin = Admin::start().await;
    for (chaos, message) in [
        (json!({"errorRate": "0.5"}), ERROR_RATE),
        (json!({"errorRate": true}), ERROR_RATE),
        (json!({"errorRate": null}), ERROR_RATE),
        (json!({"latencyRate": "x"}), LATENCY_RATE),
        (json!({"errorStatusCode": -3}), ERROR_STATUS),
        (json!({"errorStatusCode": "503"}), ERROR_STATUS_TYPE),
        (json!({"errorStatusCode": null}), ERROR_STATUS_TYPE),
        (json!({"maxLatencyMs": "x"}), MAX_LATENCY_TYPE),
        (json!({"maxLatencyMs": -1}), MAX_LATENCY),
        // Mountebank reports every field in spec order; the first is errorRate's.
        (json!({"latencyRate": "x", "errorRate": "0.5"}), ERROR_RATE),
    ] {
        admin
            .refuses(ok_with(json!([{"chaos": chaos}])), message)
            .await;
    }
}

#[tokio::test]
async fn chaos_beside_another_behavior_in_one_element_is_refused() {
    let admin = Admin::start().await;
    admin
        .refuses(
            ok_with(json!([{"chaos": {"errorRate": 1}, "wait": 5}])),
            "Each behavior object must have only one behavior type",
        )
        .await;
}

/// Every field is optional and Mountebank validates only the fields an object holds, so these are
/// all admitted, a non-integer status and an out-of-range rate included.
#[tokio::test]
async fn the_shapes_mountebank_admits_are_admitted() {
    let admin = Admin::start().await;
    for chaos in [
        json!({}),
        json!({"bogus": 1}),
        json!("x"),
        json!(5),
        json!([]),
        json!({"errorRate": 2}),
        json!({"errorRate": -1}),
        json!({"errorStatusCode": 0}),
        json!({"errorStatusCode": 1.5}),
        json!({"maxLatencyMs": 1.5}),
        json!({"errorRate": 0.5, "errorStatusCode": 503, "latencyRate": 0.1, "maxLatencyMs": 20}),
    ] {
        let port = admin.admits(ok_with(json!([{"chaos": chaos}]))).await;
        admin.stop(port).await;
    }
}

/// `compatibility.js` upcasts only wait, lookup, copy, shellTransform and decorate, then deletes
/// `_behaviors`: a `chaos` there is never validated and never runs.
#[tokio::test]
async fn chaos_in_the_object_form_is_ignored_even_when_invalid() {
    let admin = Admin::start().await;
    let invalid = json!({"is": {"body": "ok"}, "_behaviors": {"chaos": {"errorRate": "x"}}});
    let port = admin.admits(invalid).await;
    admin.stop(port).await;

    let port = admin
        .admits(json!({"is": {"body": "ok"}, "_behaviors": {"chaos": {"errorRate": 1}}}))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (200, "ok"));
    admin.stop(port).await;
}

#[tokio::test]
async fn a_chaos_element_is_written_back_as_it_was_written() {
    let admin = Admin::start().await;
    let port = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 1, "errorStatusCode": 503}}]),
        ))
        .await;
    let written = admin.definition(port).await;
    assert_eq!(
        written["stubs"][0]["responses"][0]["behaviors"],
        json!([{"chaos": {"errorRate": 1, "errorStatusCode": 503}}]),
        "{written}"
    );
    admin.stop(port).await;
}

/// Engines up to v0.25.0 stored a `chaos` element as an inert custom key, unvalidated. A
/// `--datadir` replay keeps loading it, whatever its values.
#[test]
fn a_stored_chaos_element_with_any_values_still_loads_from_a_datadir() {
    for chaos in [
        json!({"errorRate": "x"}),
        json!({"maxLatencyMs": -5}),
        json!(null),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = json!({"port": 4612, "protocol": "http", "stubs": [{"responses": [
            {"is": {}, "behaviors": [{"wait": 1}, {"chaos": chaos}]}
        ]}]});
        std::fs::write(dir.path().join("4612.json"), file.to_string()).expect("write datadir file");
        let loaded = load_configs(&ConfigSource::Dir(dir.path().to_path_buf()))
            .unwrap_or_else(|e| panic!("{chaos} must load from a datadir: {e:#}"));
        assert_eq!(loaded.len(), 1, "{chaos}");
    }
}

// ---- serving --------------------------------------------------------------------------------------

#[tokio::test]
async fn an_injected_error_keeps_the_headers_and_empties_the_body() {
    let admin = Admin::start().await;
    let port = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 1, "errorStatusCode": 503}}]),
        ))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.body, "");
    assert_eq!(
        reply.headers.get("x-a").map(|v| v.as_bytes()),
        Some(&b"1"[..])
    );
    assert_eq!(
        reply.headers.get("content-type").map(|v| v.as_bytes()),
        Some(&b"text/plain"[..])
    );
    admin.stop(port).await;
}

#[tokio::test]
async fn the_error_status_defaults_to_500_and_a_zero_status_reads_as_unset() {
    let admin = Admin::start().await;
    for chaos in [
        json!({"errorRate": 1}),
        json!({"errorRate": 1, "errorStatusCode": 0}),
    ] {
        let port = admin.admits(ok_with(json!([{"chaos": chaos}]))).await;
        let reply = admin.get(port, "/").await;
        assert_eq!((reply.status, reply.body.as_str()), (500, ""), "{chaos}");
        admin.stop(port).await;
    }
}

#[tokio::test]
async fn a_rate_of_zero_never_fires_and_a_rate_above_one_always_does() {
    let admin = Admin::start().await;
    let never = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 0, "latencyRate": 0}}]),
        ))
        .await;
    let always = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 2, "errorStatusCode": 418}}]),
        ))
        .await;
    for _ in 0..10 {
        let reply = admin.get(never, "/").await;
        assert_eq!((reply.status, reply.body.as_str()), (200, "ok"));
        assert_eq!(admin.get(always, "/").await.status, 418);
    }
    admin.stop(never).await;
    admin.stop(always).await;
}

/// Mountebank assigns the number as written and node's `writeHead` refuses it on every hit.
#[tokio::test]
async fn an_unservable_error_status_answers_node_s_invalid_status_500() {
    let admin = Admin::start().await;
    for (code, text) in [
        (json!(1.5), "1.5"),
        (json!(99), "99"),
        (json!(1000), "1000"),
    ] {
        let port = admin
            .admits(ok_with(
                json!([{"chaos": {"errorRate": 1, "errorStatusCode": code}}]),
            ))
            .await;
        let reply = admin.get(port, "/").await;
        assert_eq!(reply.status, 500, "{code}");
        let envelope: Value = serde_json::from_str(&reply.body).expect("error JSON");
        assert_eq!(
            envelope["errors"][0]["code"],
            "ERR_HTTP_INVALID_STATUS_CODE"
        );
        assert_eq!(
            envelope["errors"][0]["message"],
            format!("Invalid status code: {text}")
        );
        admin.stop(port).await;
    }
}

/// Behaviors run in array order, so a `copy` after the chaos still writes into the error response.
#[tokio::test]
async fn a_behavior_after_chaos_runs_on_the_injected_error() {
    let admin = Admin::start().await;
    let port = admin
        .admits(json!({
            "is": {"statusCode": 200, "headers": {"X-P": "${P}"}, "body": "ok"},
            "behaviors": [
                {"chaos": {"errorRate": 1, "errorStatusCode": 503}},
                {"copy": {"from": "path", "into": "${P}",
                          "using": {"method": "regex", "selector": ".+"}}}
            ]
        }))
        .await;
    let reply = admin.get(port, "/hello").await;
    assert_eq!(reply.status, 503);
    assert_eq!(
        reply.headers.get("x-p").map(|v| v.as_bytes()),
        Some(&b"/hello"[..])
    );
    admin.stop(port).await;
}

/// A later `{"chaos": null}` removes the earlier chaos step, as `null` does for every behavior.
#[tokio::test]
async fn a_null_chaos_removes_the_earlier_one() {
    let admin = Admin::start().await;
    let port = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 1}}, {"chaos": null}]),
        ))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (200, "ok"));
    admin.stop(port).await;
}

#[tokio::test]
async fn injected_latency_stays_below_max_latency_and_a_zero_max_disables_it() {
    let admin = Admin::start().await;
    let delayed = admin
        .admits(ok_with(
            json!([{"chaos": {"latencyRate": 1, "maxLatencyMs": 300}}]),
        ))
        .await;
    let mut total = Duration::ZERO;
    for _ in 0..8 {
        let started = Instant::now();
        let reply = admin.get(delayed, "/").await;
        let took = started.elapsed();
        assert_eq!((reply.status, reply.body.as_str()), (200, "ok"));
        assert!(took < Duration::from_millis(600), "one delay was {took:?}");
        total += took;
    }
    // Eight uniform draws below 300 ms all landing under 12 ms has odds near 1e-11.
    assert!(
        total > Duration::from_millis(100),
        "eight requests took {total:?}"
    );
    admin.stop(delayed).await;

    let off = admin
        .admits(ok_with(
            json!([{"chaos": {"latencyRate": 1, "maxLatencyMs": 0}}]),
        ))
        .await;
    let started = Instant::now();
    for _ in 0..8 {
        assert_eq!(admin.get(off, "/").await.status, 200);
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    admin.stop(off).await;
}

/// The rolls draw from the imposter's seeded stream (`_rift.scriptEngine.seed`), so the same seed
/// replays the same hits after a recreate.
#[tokio::test]
async fn a_seeded_imposter_replays_its_chaos() {
    let admin = Admin::start().await;
    let imposter = json!({
        "protocol": "http",
        "_rift": {"scriptEngine": {"seed": 42}},
        "stubs": [{"responses": [ok_with(json!([{"chaos": {"errorRate": 0.5, "errorStatusCode": 503}}]))]}]
    });
    let mut runs = Vec::new();
    for _ in 0..2 {
        let port = admin.admits_imposter(&imposter).await;
        let mut statuses = Vec::new();
        for _ in 0..24 {
            statuses.push(admin.get(port, "/").await.status);
        }
        admin.stop(port).await;
        runs.push(statuses);
    }
    assert!(
        runs[0].contains(&200) && runs[0].contains(&503),
        "{:?}",
        runs[0]
    );
    assert_eq!(runs[0], runs[1]);
}

/// Behaviors run on proxied responses too.
#[tokio::test]
async fn chaos_applies_to_a_proxied_response() {
    let admin = Admin::start().await;
    let origin = admin
        .admits(json!({"is": {"statusCode": 200, "body": "origin"}}))
        .await;
    let proxy = admin
        .admits(json!({
            "proxy": {"to": format!("http://127.0.0.1:{origin}"), "mode": "proxyTransparent"},
            "behaviors": [{"chaos": {"errorRate": 1, "errorStatusCode": 502}}]
        }))
        .await;
    let reply = admin.get(proxy, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (502, ""));
    admin.stop(proxy).await;
    admin.stop(origin).await;
}

/// `503.0` is the number 503 to Mountebank, so it is served, not refused at write time.
#[tokio::test]
async fn an_integral_float_error_status_is_served_as_that_status() {
    let admin = Admin::start().await;
    let port = admin
        .admits(ok_with(
            json!([{"chaos": {"errorRate": 1, "errorStatusCode": 503.0}}]),
        ))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (503, ""));
    admin.stop(port).await;
}

/// The shapes that configure nothing serve the stub's response untouched.
#[tokio::test]
async fn an_admitted_no_op_chaos_serves_the_response_unchanged() {
    let admin = Admin::start().await;
    for chaos in [
        json!({}),
        json!({"bogus": 1}),
        json!("x"),
        json!(5),
        json!([]),
        json!({"errorRate": -1}),
        json!({"errorStatusCode": 503}),
    ] {
        let port = admin.admits(ok_with(json!([{"chaos": chaos}]))).await;
        let reply = admin.get(port, "/").await;
        assert_eq!((reply.status, reply.body.as_str()), (200, "ok"), "{chaos}");
        assert_eq!(
            reply.headers.get("x-a").map(|v| v.as_bytes()),
            Some(&b"1"[..]),
            "{chaos}"
        );
        admin.stop(port).await;
    }
}

/// An injected error empties whatever body the earlier behaviors built.
#[tokio::test]
async fn an_injected_error_empties_a_body_an_earlier_copy_wrote() {
    let admin = Admin::start().await;
    let port = admin
        .admits(json!({
            "is": {"statusCode": 200, "body": "path=${P}"},
            "behaviors": [
                {"copy": {"from": "path", "into": "${P}",
                          "using": {"method": "regex", "selector": ".+"}}},
                {"chaos": {"errorRate": 1, "errorStatusCode": 503}}
            ]
        }))
        .await;
    let reply = admin.get(port, "/hello").await;
    assert_eq!((reply.status, reply.body.as_str()), (503, ""));
    admin.stop(port).await;
}

/// A written `Content-Length` follows the emptied body rather than promising the old one.
#[tokio::test]
async fn an_injected_error_corrects_a_written_content_length() {
    let admin = Admin::start().await;
    let port = admin
        .admits(json!({
            "is": {"statusCode": 200, "headers": {"Content-Length": "2"}, "body": "ok"},
            "behaviors": [{"chaos": {"errorRate": 1}}]
        }))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (500, ""));
    let length = reply
        .headers
        .get("content-length")
        .map(|v| v.as_bytes().to_vec());
    assert!(
        length.is_none() || length.as_deref() == Some(&b"0"[..]),
        "Content-Length {length:?}"
    );
    admin.stop(port).await;
}

/// Mountebank completes the response after the behaviors, and the emptied body is falsy, so the
/// imposter's `defaultResponse` body is served on an injected error.
#[tokio::test]
async fn an_injected_error_serves_the_default_response_body() {
    let admin = Admin::start().await;
    let port = admin
        .admits_imposter(&json!({
            "protocol": "http",
            "defaultResponse": {"body": "fallback"},
            "stubs": [{"responses": [
                {"is": {"body": "ok"}, "behaviors": [{"chaos": {"errorRate": 1, "errorStatusCode": 503}}]}
            ]}]
        }))
        .await;
    let reply = admin.get(port, "/").await;
    assert_eq!((reply.status, reply.body.as_str()), (503, "fallback"));
    admin.stop(port).await;
}
