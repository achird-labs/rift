//! System handlers: health, metrics, config, logs.

use crate::admin_api::types::*;
use crate::imposter::ImposterManager;
use crate::response::ErrorKind;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::sync::Arc;
use tracing::warn;

/// GET / - Root endpoint (Mountebank-compatible format)
pub fn handle_root(base_url: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({
        "_links": {
            "imposters": {"href": format!("{}/imposters", base_url)},
            "config": {"href": format!("{}/config", base_url)},
            "logs": {"href": format!("{}/logs", base_url)}
        }
    });
    json_response(StatusCode::OK, &body)
}

/// GET /health - Health check
pub fn handle_health() -> Response<Full<Bytes>> {
    json_response(StatusCode::OK, &serde_json::json!({"status": "ok"}))
}

/// GET /metrics - Prometheus metrics
pub async fn handle_metrics(manager: Arc<ImposterManager>) -> Response<Full<Bytes>> {
    let imposters = manager.list_imposters();

    let mut metrics = String::new();
    metrics.push_str("# HELP rift_imposters_total Total number of active imposters\n");
    metrics.push_str("# TYPE rift_imposters_total gauge\n");
    metrics.push_str(&format!("rift_imposters_total {}\n", imposters.len()));

    metrics.push_str("# HELP rift_imposter_requests_total Total requests per imposter\n");
    metrics.push_str("# TYPE rift_imposter_requests_total counter\n");
    for imposter in &imposters {
        if let Some(port) = imposter.config.port {
            metrics.push_str(&format!(
                "rift_imposter_requests_total{{port=\"{}\"}} {}\n",
                port,
                imposter.get_request_count()
            ));
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain; version=0.0.4")
        .body(Full::new(Bytes::from(metrics)))
        .unwrap()
}

/// GET /config - Mountebank-compatible config endpoint
///
/// `allow_injection` is threaded in explicitly (issue #342) rather than read from
/// `MB_ALLOW_INJECTION`, so an embedded host can set it without mutating process env.
pub fn handle_config(allow_injection: bool) -> Response<Full<Bytes>> {
    let config = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        // Build identity (issue #344), stamped by build.rs — the same value rift_build_info
        // reports over FFI, so one version-coherence preflight works for process and FFI modes.
        "commit": option_env!("RIFT_COMMIT"),
        "options": {
            "port": crate::admin_api::DEFAULT_ADMIN_PORT,
            "allowInjection": allow_injection,
            "localOnly": false,
            "ipWhitelist": ["*"]
        },
        "process": {
            "nodeVersion": "N/A (Rust)",
            "architecture": std::env::consts::ARCH,
            "platform": std::env::consts::OS,
            "rss": 0,
            "heapTotal": 0,
            "heapUsed": 0,
            "uptime": 0,
            "cwd": std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        }
    });
    json_response(StatusCode::OK, &config)
}

/// GET /logs - Mountebank-compatible logs endpoint
pub fn handle_logs(query: Option<&str>) -> Response<Full<Bytes>> {
    let mut start_index = 0;
    let mut end_index = 100;

    if let Some(q) = query {
        for param in q.split('&') {
            if let Some((key, value)) = param.split_once('=') {
                match key {
                    "startIndex" => {
                        if let Ok(v) = value.parse::<usize>() {
                            start_index = v;
                        }
                    }
                    "endIndex" => {
                        if let Ok(v) = value.parse::<usize>() {
                            end_index = v;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let logs = serde_json::json!({
        "logs": [],
        "_links": {
            "self": {
                "href": format!("/logs?startIndex={}&endIndex={}", start_index, end_index)
            }
        }
    });
    json_response(StatusCode::OK, &logs)
}

/// True when a reload's configs carry a scripting surface that `--allowInjection` has not
/// enabled (issue #612) — the same classifier every other imposter door uses.
fn reload_is_gated(configs: &[crate::imposter::ImposterConfig], allow_injection: bool) -> bool {
    !allow_injection
        && configs
            .iter()
            .any(crate::injection_gate::config_uses_script_surface)
}

/// POST /admin/reload - re-read the startup config source and reconcile the running imposters
/// toward it incrementally (issues #197/#316, Rift extension). No-op (200) when no
/// `--configfile`/`--datadir` was given. The new set is validated before the running imposters
/// are touched, so a parse or semantic error (bad protocol, duplicate port) returns 500 with the
/// running imposters left unchanged. Unchanged imposters keep all runtime state (recorded
/// requests, scenario state, response cyclers); only changed ports are patched or replaced.
pub async fn handle_reload(
    manager: Arc<ImposterManager>,
    config_source: Option<Arc<crate::config_loader::ConfigSource>>,
    allow_injection: bool,
) -> Response<Full<Bytes>> {
    let Some(source) = config_source else {
        return json_response(
            StatusCode::OK,
            &serde_json::json!({"message": "No config source configured; nothing to reload"}),
        );
    };

    // Parse before touching state — a bad config leaves the running imposters intact.
    //
    // On the blocking pool (issue #550): `load_configs` is synchronous — many small `std::fs`
    // reads plus EJS/regex/serde work — and unlike startup, reload is network-triggered and
    // repeatable, so a large datadir or a stalled mount would pin a runtime worker for the whole
    // read. Awaited here, so the parse still completes before anything mutates.
    let loaded =
        match tokio::task::spawn_blocking(move || crate::config_loader::load_configs_full(&source))
            .await
        {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(e)) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Reload failed (imposters unchanged): {e}"),
                );
            }
            Err(e) => {
                return error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Reload task failed: {e}"),
                );
            }
        };

    // The `intercept` block is applied at boot only (issue #655): re-seeding would duplicate or
    // clobber rules added at runtime, and rebinding the listener is a lifecycle change reload does
    // not own. The party that needs to know is the client that just asked for the reload, and a
    // server-side log is invisible to it (doubly so over FFI, where tracing may go nowhere) — so
    // the response body carries the warning and the log is a `warn!`, not an `info!`: the caller
    // asked for something that deliberately did not happen.
    let warnings: Vec<String> = if loaded.intercept.is_some() {
        let warning = "the config file's `intercept` block is applied at startup only and was NOT \
                       re-applied; imposters were reloaded. Use /intercept/rules to change rules \
                       at runtime, or restart to re-read the block.";
        warn!("Reload: {warning}");
        vec![warning.to_string()]
    } else {
        Vec::new()
    };
    let configs = loaded.imposters;

    // Validate-before-touch: refuse a gated config before anything mutates, so a refused reload
    // leaves the running imposters exactly as they were (issue #612).
    if reload_is_gated(&configs, allow_injection) {
        return crate::admin_api::handlers::imposters::injection_disallowed_response();
    }

    let count = configs.len();
    match manager.apply_config(configs).await {
        Ok(report) if report.failed.is_empty() => {
            let mut body = serde_json::json!({
                "message": format!("Reloaded {count} imposter(s)"),
                "created": report.created,
                "replaced": report.replaced,
                "stubPatched": report.stub_patched,
                "deleted": report.deleted,
            });
            // Additive: absent entirely when there is nothing to warn about, so existing clients
            // and their parsers are unaffected.
            if !warnings.is_empty() {
                body["warnings"] = serde_json::json!(warnings);
            }
            json_response(StatusCode::OK, &body)
        }
        // A reload failure is a server-side config problem, not a bad client request — report 5xx.
        // Validation errors are caught before anything mutates (running imposters intact, Err
        // below); per-port failures here are residual apply errors (e.g. a bind failure on a
        // freed port) with the other ports already reconciled — so the body carries the full
        // report: a partial failure is exactly when the client needs to know what did apply.
        Ok(report) => {
            let failures: Vec<String> = report
                .failed
                .iter()
                .map(|(port, e)| match port {
                    // Port 0 is the ApplyReport sentinel for auto-assigned (port-less) configs.
                    0 => format!("auto-assign: {e}"),
                    port => format!("{port}: {e}"),
                })
                .collect();
            // Mountebank error envelope (like `error_response`) plus the apply report.
            json_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &serde_json::json!({
                    "errors": [{
                        "code": StatusCode::INTERNAL_SERVER_ERROR.as_str(),
                        "type": ErrorKind::InternalError.slug(),
                        "message": format!("Reload partially failed: {}", failures.join("; ")),
                    }],
                    "failed": failures,
                    "created": report.created,
                    "replaced": report.replaced,
                    "stubPatched": report.stub_patched,
                    "deleted": report.deleted,
                }),
            )
        }
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Reload failed (imposters unchanged): {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handle_root() {
        let resp = handle_root("http://localhost:2525");
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_handle_health() {
        let resp = handle_health();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_handle_config() {
        let resp = handle_config(false);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // AC2 (#344): GET /config exposes a `commit` build-identity field (present even when unstamped,
    // i.e. JSON null); in a git checkout build.rs stamps it to HEAD, matching rift_build_info.
    #[test]
    fn handle_config_reports_commit() {
        use http_body_util::BodyExt;
        let resp = handle_config(false);
        let bytes = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resp.into_body().collect())
            .unwrap()
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json.get("commit").is_some(),
            "GET /config exposes a commit field (string or null)"
        );
        if let Some(commit) = json["commit"].as_str() {
            // Mirror build.rs: the RIFT_COMMIT override wins (CI), else `git rev-parse HEAD`.
            let expected = std::env::var("RIFT_COMMIT").unwrap_or_else(|_| {
                let out = std::process::Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .expect("git rev-parse");
                String::from_utf8(out.stdout).unwrap().trim().to_owned()
            });
            assert_eq!(
                commit, expected,
                "stamped commit matches HEAD (or the env override)"
            );
        }
    }

    // AC7 (#342): the injection flag is threaded explicitly, not read from process env —
    // an embedder sets it without mutating the environment.
    #[test]
    fn handle_config_reports_explicit_injection_flag() {
        use http_body_util::BodyExt;
        let read_flag = |allow: bool| {
            let resp = handle_config(allow);
            let bytes = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(resp.into_body().collect())
                .unwrap()
                .to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            json["options"]["allowInjection"].as_bool().unwrap()
        };
        assert!(read_flag(true), "explicit true must be reported");
        assert!(!read_flag(false), "explicit false must be reported");
    }

    #[tokio::test]
    async fn test_handle_reload_no_source_is_noop() {
        let manager = Arc::new(ImposterManager::new());
        let resp = handle_reload(manager, None, false).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Issue #316: a partial apply failure returns 500 with the full report, so the client
    // can tell what state the server is now in.
    #[tokio::test]
    async fn test_handle_reload_partial_failure_returns_report() {
        use http_body_util::BodyExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters":[
                {"port":19477,"protocol":"http","stubs":[]},
                {"port":19478,"protocol":"https","cert":"not a pem","key":"not a pem","stubs":[]}
            ]}"#,
        )
        .expect("write config");
        let source = Arc::new(crate::config_loader::ConfigSource::File {
            path,
            no_parse: false,
        });

        let manager = Arc::new(ImposterManager::new());
        let resp = handle_reload(manager.clone(), Some(source), false).await;
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert!(
            body["errors"][0]["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("Reload partially failed: 19478:"),
            "got: {body}"
        );
        assert_eq!(
            body["created"],
            serde_json::json!([19477]),
            "sibling applied"
        );
        assert!(manager.get_imposter(19477).is_ok());
        assert!(manager.get_imposter(19478).is_err());

        manager.delete_all().await;
    }

    // Issue #356: editing a `file:`-referenced script and reloading must pick up the new
    // content — the resolve-scripts pass has to re-run on every reload (it does, since reload
    // re-reads the config source from scratch via `config_loader::load_configs`, which resolves
    // scripts as part of parsing), not just once at startup.
    #[tokio::test]
    async fn test_handle_reload_picks_up_edited_file_referenced_script() {
        use crate::imposter::StubResponse;

        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("respond.rhai");
        std::fs::write(&script_path, r#"fn respond(ctx) { http(503, "first") }"#)
            .expect("write script (first version)");

        let config_path = dir.path().join("imposter.json");
        std::fs::write(
            &config_path,
            r#"{"port":19479,"protocol":"http","stubs":[{"responses":[{"_rift":{"script":{"file":"respond.rhai"}}}]}]}"#,
        )
        .expect("write config");

        let source = Arc::new(crate::config_loader::ConfigSource::File {
            path: config_path,
            no_parse: false,
        });

        let manager = Arc::new(ImposterManager::new());

        // Initial reload creates the imposter with the first script version resolved.
        let resp = handle_reload(manager.clone(), Some(source.clone()), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let script_code = |manager: &ImposterManager| {
            let imposter = manager.get_imposter(19479).expect("imposter exists");
            let stubs = imposter.get_stubs();
            match &stubs[0].responses[0] {
                StubResponse::RiftScript { rift } => {
                    rift.script.as_ref().and_then(|s| s.code.clone())
                }
                other => panic!("expected RiftScript response, got {other:?}"),
            }
        };
        assert_eq!(
            script_code(&manager).as_deref(),
            Some(r#"fn respond(ctx) { http(503, "first") }"#)
        );

        // Edit the referenced file (the configfile itself is untouched) and reload again.
        std::fs::write(&script_path, r#"fn respond(ctx) { http(503, "second") }"#)
            .expect("write script (second version)");
        let resp = handle_reload(manager.clone(), Some(source), true).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            script_code(&manager).as_deref(),
            Some(r#"fn respond(ctx) { http(503, "second") }"#),
            "reload must pick up the edited file content"
        );

        manager.delete_all().await;
    }

    #[test]
    fn test_handle_logs_no_query() {
        let resp = handle_logs(None);
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_handle_logs_with_pagination() {
        let resp = handle_logs(Some("startIndex=10&endIndex=50"));
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ===== Issue #612: POST /admin/reload is the third config door and must gate too =====

    /// Reload from `path`, then read back whether port 19481's stub carries a decorate behavior.
    #[tokio::test]
    async fn test_handle_reload_rejects_injection_when_flag_off() {
        use http_body_util::BodyExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19481,"protocol":"http","stubs":[
                {"responses":[{"is":{"statusCode":200,"body":"original"}}]}
            ]}]}"#,
        )
        .expect("write clean config");
        let source = Arc::new(crate::config_loader::ConfigSource::File {
            path: path.clone(),
            no_parse: false,
        });

        let manager = Arc::new(ImposterManager::new());
        let resp = handle_reload(manager.clone(), Some(source.clone()), false).await;
        assert_eq!(resp.status(), StatusCode::OK, "clean config reloads");
        assert!(manager.get_imposter(19481).is_ok());

        // Edit the config to add a decorate behavior — a script surface.
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19481,"protocol":"http","stubs":[
                {"responses":[{"is":{"statusCode":200,"body":"decorated"},
                 "_behaviors":{"decorate":"function (req, res) { res.body = 'pwned'; }"}}]}
            ]}]}"#,
        )
        .expect("write scripted config");

        let resp = handle_reload(manager.clone(), Some(source), false).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "a decorate behavior without --allowInjection must be refused on reload"
        );
        let bytes = resp.into_body().collect().await.expect("body").to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json body");
        assert_eq!(
            body["errors"][0]["code"], "invalid injection",
            "must use the Mountebank envelope: {body}"
        );

        // Validate-before-touch: the running imposter is untouched by the refused reload.
        let imposter = manager.get_imposter(19481).expect("imposter still running");
        let stubs = imposter.get_stubs();
        let body_of = |r: &crate::imposter::StubResponse| match r {
            crate::imposter::StubResponse::Is { is, .. } => is.body.clone(),
            other => panic!("expected an Is response, got {other:?}"),
        };
        assert_eq!(
            body_of(&stubs[0].responses[0]),
            Some(serde_json::json!("original")),
            "the refused reload must leave the running imposter unchanged"
        );

        manager.delete_all().await;
    }

    // AC8: with the flag set, the same scripted reload applies.
    #[tokio::test]
    async fn test_handle_reload_applies_injection_when_flag_set() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("imposters.json");
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19482,"protocol":"http","stubs":[
                {"responses":[{"is":{"statusCode":200,"body":"decorated"},
                 "_behaviors":{"decorate":"function (req, res) { res.body = 'ok'; }"}}]}
            ]}]}"#,
        )
        .expect("write scripted config");
        let source = Arc::new(crate::config_loader::ConfigSource::File {
            path,
            no_parse: false,
        });

        let manager = Arc::new(ImposterManager::new());
        let resp = handle_reload(manager.clone(), Some(source), true).await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "--allowInjection must permit a decorate behavior on reload"
        );
        assert!(manager.get_imposter(19482).is_ok());

        manager.delete_all().await;
    }
}
