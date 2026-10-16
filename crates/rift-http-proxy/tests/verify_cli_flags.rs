//! Issue #1492 (ported from rift-conformance's RiftToolsCli204 spec): `rift-verify`'s request
//! flags end to end — `-p`, `--status-only`, `--dry-run`, `-c` and `--gateway` — against an
//! in-process admin API, asserting the summary counts and what the imposter actually recorded.

use rift_http_proxy::imposter::ImposterManager;
use std::sync::Arc;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_rift-verify");

/// One recorded static stub: `GET /st` → 201 "b".
fn static_imposter() -> serde_json::Value {
    serde_json::json!({
        "protocol": "http", "recordRequests": true,
        "stubs": [{
            "predicates": [{ "equals": { "path": "/st" } }],
            "responses": [{ "is": { "statusCode": 201, "body": "b" } }]
        }]
    })
}

struct Fixture {
    manager: Arc<ImposterManager>,
    admin_url: String,
}

impl Fixture {
    async fn start() -> Self {
        let manager = Arc::new(ImposterManager::new());
        let running = rift_http_proxy::admin_api::AdminApiServer::new(
            "127.0.0.1:0".parse().expect("addr"),
            Arc::clone(&manager),
            None,
        )
        .bind()
        .await
        .expect("admin API binds");
        let admin_url = format!("http://{}", running.local_addr());
        Self { manager, admin_url }
    }

    async fn create(&self, config: serde_json::Value) -> u16 {
        let config = serde_json::from_value(config).expect("valid imposter config");
        self.manager
            .create_imposter(config)
            .await
            .expect("create imposter")
    }

    /// Run rift-verify against this admin API with `args`; returns (exit code, stdout, stderr).
    async fn verify(&self, args: &[&str]) -> (Option<i32>, String, String) {
        let out = Command::new(BIN)
            .args(["--admin-url", self.admin_url.as_str()])
            .args(args)
            .output()
            .await
            .expect("run rift-verify");
        (
            out.status.code(),
            String::from_utf8(out.stdout).expect("utf8 stdout"),
            String::from_utf8(out.stderr).expect("utf8 stderr"),
        )
    }

    /// Run rift-verify with `-o json` and parse its summary.
    async fn summary(&self, args: &[&str]) -> serde_json::Value {
        let mut argv = args.to_vec();
        argv.extend_from_slice(&["-o", "json"]);
        let (_, stdout, stderr) = self.verify(&argv).await;
        serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("summary is not JSON ({e}): {stdout}\nstderr: {stderr}"))
    }

    fn recorded_from(&self, port: u16) -> Vec<String> {
        self.manager
            .get_imposter(port)
            .expect("imposter")
            .get_recorded_requests()
            .iter()
            .map(|r| r.request_from.clone())
            .collect()
    }
}

fn counts(summary: &serde_json::Value) -> (u64, u64, u64) {
    let n = |k: &str| {
        summary[k]
            .as_u64()
            .unwrap_or_else(|| panic!("numeric `{k}`: {summary}"))
    };
    (n("imposters"), n("passed"), n("failed"))
}

#[tokio::test]
async fn port_flag_limits_verification_to_one_imposter() {
    let fx = Fixture::start().await;
    let port = fx.create(static_imposter()).await;
    fx.create(static_imposter()).await;

    let one = fx.summary(&["-p", &port.to_string()]).await;
    assert_eq!(counts(&one), (1, 1, 0), "{one}");
    let all = fx.summary(&[]).await;
    assert_eq!(counts(&all), (2, 2, 0), "{all}");
}

/// The second stub is shadowed by the first, so its own request gets the first stub's body: a
/// body mismatch that `--status-only` ignores.
#[tokio::test]
async fn status_only_passes_a_stub_whose_body_differs() {
    let fx = Fixture::start().await;
    let port = fx
        .create(serde_json::json!({
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/a" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "a" } }] },
                { "predicates": [{ "startsWith": { "path": "/a" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "shadowed" } }] }
            ]
        }))
        .await;
    let p = port.to_string();

    let full = fx.summary(&["-p", &p]).await;
    assert_eq!(counts(&full), (1, 1, 1), "{full}");
    let status_only = fx.summary(&["-p", &p, "--status-only"]).await;
    assert_eq!(counts(&status_only), (1, 2, 0), "{status_only}");
}

#[tokio::test]
async fn dry_run_sends_nothing() {
    let fx = Fixture::start().await;
    let port = fx.create(static_imposter()).await;

    let dry = fx.summary(&["-p", &port.to_string(), "--dry-run"]).await;
    assert_eq!(dry["failed"], 0, "{dry}");
    assert_eq!(fx.recorded_from(port), Vec::<String>::new());
}

#[tokio::test]
async fn show_curl_prints_the_request_it_sends() {
    let fx = Fixture::start().await;
    let port = fx.create(static_imposter()).await;

    let (code, stdout, stderr) = fx.verify(&["-p", &port.to_string(), "-c"]).await;
    let all = format!("{stdout}{stderr}");
    assert_eq!(code, Some(0), "{all}");
    assert!(
        all.contains(&format!("curl -X GET 'http://localhost:{port}/st'")),
        "{all}"
    );
    assert_eq!(
        fx.recorded_from(port).len(),
        1,
        "-c still sends the request"
    );
}

/// `--gateway` routes through `{admin}/__rift/<port>/…`, which records a `:0` requestFrom where a
/// direct request records the client's real ephemeral port.
#[tokio::test]
async fn gateway_sends_through_the_admin_gateway() {
    let fx = Fixture::start().await;
    let port = fx.create(static_imposter()).await;
    let p = port.to_string();

    let direct = fx.summary(&["-p", &p]).await;
    assert_eq!(counts(&direct), (1, 1, 0), "{direct}");
    let gateway = fx.summary(&["-p", &p, "--gateway"]).await;
    assert_eq!(counts(&gateway), (1, 1, 0), "{gateway}");

    let from = fx.recorded_from(port);
    assert_eq!(from.len(), 2, "{from:?}");
    assert!(!from[0].ends_with(":0"), "direct request: {from:?}");
    assert_eq!(from[1], "127.0.0.1:0", "gateway request: {from:?}");
}
