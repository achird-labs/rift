//! Issue #1758: `_rift.script.export: "http"` serves a standard `wasi:http/proxy` component. The
//! component is `examples/wasm/http-twin`, built by a real wasm32-wasip2 toolchain (its source
//! lists what each path answers), in two builds: the default one also exports `rift:mock/handler`,
//! the `--no-default-features` one exports `wasi:http/incoming-handler` only.
//!
//! Ignored by default: they need the component built first. The CI `wasm` job builds both and runs
//! this test with `--ignored`:
//!
//! ```sh
//! cargo build --release --target wasm32-wasip2 --manifest-path examples/wasm/http-twin/Cargo.toml
//! cargo build --release --target wasm32-wasip2 --manifest-path examples/wasm/http-twin/Cargo.toml \
//!   --no-default-features --target-dir examples/wasm/http-twin/target/http-only
//! cargo test -p rift-http-proxy --test wasm_http_export -- --ignored
//! ```
#![cfg(feature = "wasm")]

use base64::Engine as _;
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use serde_json::{Value, json};
use std::path::PathBuf;

const IGNORE: &str =
    "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it";

fn twin_path(http_only: bool) -> PathBuf {
    let target = if http_only {
        "target/http-only/wasm32-wasip2/release/http_twin.wasm"
    } else {
        "target/wasm32-wasip2/release/http_twin.wasm"
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/wasm/http-twin")
        .join(target)
}

/// The component, base64, as resolution writes a `.wasm` file into `_rift.script.code`. Compiled
/// once here, as admitting it through the admin API would: these tests create imposters directly,
/// and a dozen first requests compiling the same component at once can miss their deadline.
fn twin(http_only: bool) -> String {
    static COMPILED: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());
    let path = twin_path(http_only);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("build the example first: {}: {e}", path.display()));
    let code = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut compiled = COMPILED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !compiled.contains(&http_only) {
        rift_http_proxy::scripting::admit_wasm_component(&code, None).expect("the twin compiles");
        compiled.push(http_only);
    }
    code
}

/// An imposter whose one stub runs the component under `script` (merged over `engine`/`code`).
fn imposter(code: &str, script: Value, engine: Value) -> ImposterConfig {
    let mut script_config = json!({ "engine": "wasm", "code": code });
    if let (Some(base), Some(extra)) = (script_config.as_object_mut(), script.as_object()) {
        base.extend(extra.clone());
    }
    serde_json::from_value(json!({
        "protocol": "http",
        "_rift": { "scriptEngine": engine },
        "stubs": [{ "responses": [{ "_rift": { "script": script_config } }] }]
    }))
    .expect("config")
}

struct Served {
    manager: ImposterManager,
    port: u16,
    client: reqwest::Client,
}

impl Served {
    async fn new(config: ImposterConfig) -> Self {
        let manager = ImposterManager::new();
        let port = manager.create_imposter(config).await.expect("create");
        Self {
            manager,
            port,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.expect("get")
    }

    async fn stop(self) {
        self.manager
            .delete_imposter(self.port)
            .await
            .expect("delete");
    }
}

fn header_values(resp: &reqwest::Response, name: &str) -> Vec<String> {
    resp.headers()
        .get_all(name)
        .iter()
        .map(|v| v.to_str().expect("text").to_owned())
        .collect()
}

const BINARY: &[u8] = b"payload\x01\x02\x00\xff\xfe";

#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_binary_body_and_repeated_headers_are_served_byte_exact() {
    let _ = IGNORE;
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    // Content-Length framing.
    let resp = served
        .client
        .post(served.url("/echo?x=1"))
        .header("content-type", "application/octet-stream")
        .body(BINARY)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), 200);
    assert_eq!(header_values(&resp, "x-echo-method"), ["POST"]);
    assert_eq!(header_values(&resp, "set-cookie"), ["a=1", "b=2"]);
    assert_eq!(header_values(&resp, "x-rift-script"), ["wasm"]);
    let mut expected = b"POST /echo?x=1\n".to_vec();
    expected.extend_from_slice(BINARY);
    assert_eq!(
        resp.bytes().await.expect("body").as_ref(),
        expected.as_slice()
    );
    // Chunked framing: the same bytes, streamed.
    let chunks: Vec<Result<&'static [u8], std::io::Error>> =
        vec![Ok(&BINARY[..4]), Ok(&BINARY[4..])];
    let resp = served
        .client
        .post(served.url("/echo?x=1"))
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .expect("post chunked");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.bytes().await.expect("body").as_ref(),
        expected.as_slice()
    );
    served.stop().await;
}

#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn knobs_reach_an_http_component_as_rift_knobs() {
    let knobs = json!({ "mode": "slow", "n": 3 });
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http", "knobs": knobs }),
        json!({}),
    ))
    .await;
    let resp = served.get("/echo").await;
    assert_eq!(header_values(&resp, "x-knobs"), [knobs.to_string()]);
    served.stop().await;
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    assert_eq!(header_values(&served.get("/echo").await, "x-knobs"), ["{}"]);
    served.stop().await;
}

// The same component under both exports answers the same echo: "same component, two uses".
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn one_component_answers_alike_under_both_exports() {
    let code = twin(false);
    let mut answers = Vec::new();
    for export in ["http", "rift"] {
        let served = Served::new(imposter(
            &code,
            json!({ "export": export, "knobs": { "k": 1 } }),
            json!({}),
        ))
        .await;
        let resp = served
            .client
            .post(served.url("/echo?x=1"))
            .body("payload")
            .send()
            .await
            .expect("post");
        let status = resp.status().as_u16();
        let method = header_values(&resp, "x-echo-method");
        let knobs = header_values(&resp, "x-knobs");
        let body = resp.bytes().await.expect("body");
        answers.push((status, method, knobs, body));
        served.stop().await;
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0].3.as_ref(), b"POST /echo?x=1\npayload");
    // Unset on a component exporting both: rift.
    let served = Served::new(imposter(&code, json!({}), json!({}))).await;
    assert!(header_values(&served.get("/echo").await, "set-cookie").is_empty());
    served.stop().await;
}

#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn an_http_only_component_is_inferred_and_refuses_the_rift_export() {
    let code = twin(true);
    // No `export`: the component exports only wasi:http/incoming-handler, so it is http.
    let served = Served::new(imposter(&code, json!({}), json!({}))).await;
    let resp = served.get("/echo").await;
    assert_eq!(header_values(&resp, "set-cookie"), ["a=1", "b=2"]);
    served.stop().await;
    // Admission names what the component does export; it imports wasi:http/types and is admitted.
    assert_eq!(
        rift_http_proxy::scripting::admit_wasm_component(&code, Some("http")),
        Ok("http")
    );
    let err = rift_http_proxy::scripting::admit_wasm_component(&code, Some("rift"))
        .expect_err("no rift export");
    assert!(
        err.contains("export \"rift\" needs rift:mock/handler")
            && err.contains("it exports wasi:http/incoming-handler"),
        "{err}"
    );
}

// The body is read while `handle` runs: `/stream` sets the response first and writes after.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_body_written_after_the_response_is_set_is_served_whole() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    let resp = served.get("/stream").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.expect("body"), "one-two-three");
    served.stop().await;
}

#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn no_response_an_error_response_and_a_trap_are_script_errors() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    for (path, message) in [
        ("/none", "component returned no response"),
        ("/error", "component answered an error"),
        ("/trap", "wasm trap"),
    ] {
        let resp = served.get(path).await;
        assert_eq!(resp.status(), 500, "{path}");
        assert_eq!(
            header_values(&resp, "x-rift-script-error"),
            ["true"],
            "{path}"
        );
        let body = resp.text().await.expect("body");
        assert!(body.contains(message), "{path}: {body}");
    }
    served.stop().await;
}

// The body is bounded by `maxMemoryMiB`: past it is an error, not a truncated 200.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_response_body_past_max_memory_is_a_script_error() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({ "wasm": { "maxMemoryMiB": 16 } }),
    ))
    .await;
    let resp = served.get("/big?bytes=1048576").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.expect("body").len(), 1 << 20);
    let resp = served.get(&format!("/big?bytes={}", 17 << 20)).await;
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("response body exceeds maxMemoryMiB (16 MiB)"),
        "{body}"
    );
    served.stop().await;
}

// An http component reaches the imposter's flow store through rift:mock/state.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn an_http_component_keeps_state_in_the_flow_store() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    for n in 1..=3 {
        let resp = served.get("/count").await;
        assert_eq!(
            resp.text().await.expect("body"),
            format!(r#"{{"count":{n}}}"#)
        );
    }
    // A fresh instance per request: guest memory does not carry over.
    for _ in 0..2 {
        let resp = served.get("/memory").await;
        assert_eq!(resp.text().await.expect("body"), r#"{"count":1}"#);
    }
    served.stop().await;
}

// A guest that declares a length and returns with its body cut short is an error, not a 200 with
// what was written; a right length is served, framed by Rift.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_declared_length_must_match_the_body() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    let resp = served.get("/length-ok").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(header_values(&resp, "content-length"), ["3"]);
    assert_eq!(resp.text().await.expect("body"), "abc");
    let resp = served.get("/length-cut").await;
    assert_eq!(resp.status(), 500);
    let body = resp.text().await.expect("body");
    assert!(body.contains("but its content-length is"), "{body}");
    // Bytes that are not UTF-8 are served as they are.
    let resp = served.get("/bytes").await;
    assert_eq!(resp.bytes().await.expect("body").as_ref(), b"\xff\x00\xfe");
    served.stop().await;
}

// A guest that sets its response and then spins is cut at the deadline: a 504, not the partial body.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_spinning_http_component_times_out() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({ "timeoutMs": 300 }),
    ))
    .await;
    let started = std::time::Instant::now();
    let resp = served.get("/spin").await;
    assert_eq!(resp.status(), 504);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    served.stop().await;
}

// The query reaches the component as the client sent it: order, repeats and encoding kept.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn the_query_reaches_the_component_unchanged() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http" }),
        json!({}),
    ))
    .await;
    let resp = served.get("/echo?b=2&a=1&b=%20x&flag").await;
    assert_eq!(
        resp.text().await.expect("body"),
        "GET /echo?b=2&a=1&b=%20x&flag\n"
    );
    served.stop().await;
}

// Issue #1793: under `instance: "imposter"` an http component's guest memory lasts across requests.
#[tokio::test]
#[ignore = "needs examples/wasm/http-twin built for wasm32-wasip2; the CI wasm job runs it"]
async fn a_kept_http_instance_counts_in_guest_memory() {
    let served = Served::new(imposter(
        &twin(false),
        json!({ "export": "http", "instance": "imposter" }),
        json!({}),
    ))
    .await;
    for n in 1..=3 {
        let resp = served.get("/memory").await;
        assert_eq!(
            resp.text().await.expect("body"),
            format!(r#"{{"count":{n}}}"#)
        );
    }
    served.stop().await;
}
