//! Issue #204 gate: drive the full lifecycle across the C-ABI from a Rust integration test —
//! start → create imposter (JSON) → replace stubs (JSON) → serve+record → recorded (JSON) →
//! free → delete_all → stop — plus the null-pointer and error-sentinel paths. (Double-free /
//! use-after-free are undefined behaviour, so they belong under Miri/ASan, not a normal test.)

use rift_ffi::*;
use std::ffi::{CStr, CString, c_char};

fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior NUL in test input")
}

unsafe fn take_json(p: *mut c_char) -> String {
    unsafe {
        assert!(!p.is_null(), "expected JSON, got null");
        let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
        rift_free(p);
        s
    }
}

#[test]
fn ffi_round_trip() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null(), "rift_start returned null");

        // create imposter from JSON config; returns its port
        let config =
            cstr(r#"{ "port": 19990, "protocol": "http", "recordRequests": true, "stubs": [] }"#);
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19990);

        // replace stubs from a JSON array
        let stubs = cstr(
            r#"[{ "predicates": [{ "equals": { "path": "/hello" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "world" } }] }]"#,
        );
        assert_eq!(rift_replace_stubs(h, port, stubs.as_ptr()), 0);

        // the embedded mock serves on its port — drive it like any HTTP service
        let rt = tokio::runtime::Runtime::new().unwrap();
        let body = rt.block_on(async {
            let r = reqwest::get("http://127.0.0.1:19990/hello")
                .await
                .expect("get");
            assert_eq!(r.status(), 200);
            r.text().await.expect("text")
        });
        assert_eq!(body, "world");

        // recorded requests come back as JSON; caller frees the buffer
        let recorded = take_json(rift_recorded(h, port));
        let parsed: serde_json::Value = serde_json::from_str(&recorded).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["path"], "/hello");

        // teardown removes all imposters — verified by the effect, not just the return code:
        // the port is gone, so recorded now returns null.
        assert_eq!(rift_delete_all(h), 0);
        assert!(
            rift_recorded(h, port).is_null(),
            "delete_all must remove the imposter (recorded → null)"
        );

        // stop drops the handle + runtime
        rift_stop(h);
    }
}

/// Issue #494: server-side verification over the C-ABI — record two requests, then count matches
/// through `rift_verify` (the engine's one true predicate evaluator) and inspect the closest
/// non-match diff.
#[test]
fn ffi_verify_round_trip() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null());

        let config =
            cstr(r#"{ "port": 19997, "protocol": "http", "recordRequests": true, "stubs": [] }"#);
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19997);

        let stubs = cstr(
            r#"[{ "predicates": [], "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }]"#,
        );
        assert_eq!(rift_replace_stubs(h, port, stubs.as_ptr()), 0);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            reqwest::get("http://127.0.0.1:19997/wanted")
                .await
                .expect("get");
            reqwest::get("http://127.0.0.1:19997/other")
                .await
                .expect("get");
        });

        // Count requests to /wanted, asking for the closest non-match diff.
        let body = cstr(
            r#"{"predicates":[{"equals":{"path":"/wanted"}}],"includeRequests":true,"includeClosest":true}"#,
        );
        let out = take_json(rift_verify(h, port, body.as_ptr()));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["matched"], 1);
        assert_eq!(v["total"], 2);
        assert_eq!(v["requests"].as_array().unwrap().len(), 1);
        assert_eq!(v["requests"][0]["path"], "/wanted");
        // The closest non-match is /other, failing the single path clause.
        assert_eq!(v["closest"]["request"]["path"], "/other");
        assert_eq!(
            v["closest"]["failedPredicates"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            v["closest"]["failedPredicates"][0]["actual"]["path"],
            "/other"
        );

        // Error paths: null handle/body, unknown port, malformed JSON → null (+ last_error).
        assert!(rift_verify(std::ptr::null_mut(), port, body.as_ptr()).is_null());
        assert!(rift_verify(h, port, std::ptr::null()).is_null());
        assert!(
            rift_verify(h, 12345, body.as_ptr()).is_null(),
            "unknown port → null"
        );
        let bad = cstr("not json");
        assert!(
            rift_verify(h, port, bad.as_ptr()).is_null(),
            "malformed JSON → null"
        );

        assert_eq!(rift_delete_all(h), 0);
        rift_stop(h);
    }
}

#[test]
fn ffi_null_and_error_paths() {
    unsafe {
        // null handle / null json never abort — they return error sentinels
        assert_eq!(
            rift_create_imposter(std::ptr::null_mut(), std::ptr::null()),
            0
        );
        assert_eq!(
            rift_replace_stubs(std::ptr::null_mut(), 1, std::ptr::null()),
            -1
        );
        assert_eq!(rift_delete_all(std::ptr::null_mut()), -1);
        assert!(rift_recorded(std::ptr::null_mut(), 1).is_null());
        // freeing null and stopping null are no-ops
        rift_free(std::ptr::null_mut());
        rift_stop(std::ptr::null_mut());

        // a live handle with malformed JSON / unknown port also yields sentinels
        let h = rift_start();
        let bad = cstr("not json");
        assert_eq!(rift_create_imposter(h, bad.as_ptr()), 0);
        assert_eq!(rift_replace_stubs(h, 12345, bad.as_ptr()), -1);
        // valid stubs but an unknown port exercises the manager's own NotFound error path
        let valid = cstr(r#"[{ "predicates": [], "responses": [] }]"#);
        assert_eq!(
            rift_replace_stubs(h, 12345, valid.as_ptr()),
            -1,
            "unknown port → -1"
        );
        assert!(rift_recorded(h, 12345).is_null(), "unknown port → null");
        rift_stop(h);
    }
}

// ===========================================================================
// Issue #343: C-ABI v2 — in-process admin plane, apply_config, delete_imposter,
// build_info, last_error.
// ===========================================================================

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

/// The commit build.rs would stamp: the RIFT_COMMIT override if set (CI), else `git rev-parse HEAD`.
fn expected_commit() -> String {
    std::env::var("RIFT_COMMIT").unwrap_or_else(|_| {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    })
}

/// Call `rift_serve_admin`, assert it succeeded, and parse the JSON (frees the buffer).
unsafe fn serve_admin(h: *mut RiftHandle, opts: &str) -> serde_json::Value {
    unsafe {
        let c = cstr(opts);
        let json = take_json(rift_serve_admin(h, c.as_ptr()));
        serde_json::from_str(&json).expect("serve_admin returns JSON")
    }
}

// AC1: rift_serve_admin spawns the real admin API over the handle's manager — an FFI-created
// imposter is visible via GET /imposters, the admin control plane creates imposters that serve
// data-plane traffic, and scenario endpoints respond.
#[test]
fn ffi_serve_admin_exposes_manager_and_control_plane() {
    unsafe {
        let h = rift_start();
        let cfg = cstr(
            r#"{"port":19991,"protocol":"http","stubs":[{"predicates":[{"equals":{"path":"/ping"}}],"responses":[{"is":{"statusCode":200,"body":"pong"}}]}]}"#,
        );
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19991);

        let info = serve_admin(h, "{}");
        assert!(info["adminPort"].as_u64().expect("adminPort") > 0);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            // The FFI-created imposter is visible over the real admin API.
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("admin /imposters reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("imposters array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(
                ports.contains(&19991),
                "FFI imposter visible via admin /imposters"
            );

            // Control plane: create an imposter over admin; it serves data-plane traffic.
            let create = reqwest::Client::new()
                .post(format!("{admin_url}/imposters"))
                .json(&serde_json::json!({
                    "port":19992,"protocol":"http",
                    "stubs":[{"responses":[{"is":{"statusCode":201,"body":"made"}}]}]
                }))
                .send()
                .await
                .expect("admin create imposter");
            assert!(
                create.status().is_success(),
                "admin created imposter over the embedded plane"
            );

            let served = reqwest::get("http://127.0.0.1:19992/anything")
                .await
                .expect("admin-created imposter reachable");
            assert_eq!(served.status(), 201);

            // Scenario endpoint responds (spot-check the wider control-plane surface).
            let scen = reqwest::get(format!("{admin_url}/imposters/19991/scenarios"))
                .await
                .expect("scenarios endpoint reachable");
            assert_eq!(
                scen.status(),
                200,
                "scenario endpoint responds over the embedded admin"
            );
        });

        rift_stop(h);
    }
}

// Issue #492: `allowInjection` in the rift_serve_admin options gates script imposters submitted
// THROUGH the admin plane. With it set, an inject imposter POSTed to the admin API is accepted;
// with the default (off), the same POST is rejected with the Mountebank "invalid injection" error.
// (Direct FFI creation is ungated by design and is not exercised here.)
#[test]
fn ffi_serve_admin_allow_injection_gates_the_admin_plane() {
    unsafe {
        let inject_imposter = serde_json::json!({
            "port": 19972, "protocol": "http",
            "stubs": [{ "responses": [{ "inject": "function (config) { return { statusCode: 200, body: 'hi' }; }" }] }]
        });

        // allowInjection: true → the admin plane accepts the script imposter.
        let h_on = rift_start();
        let admin_on = serve_admin(h_on, r#"{"allowInjection": true}"#);
        let url_on = admin_on["adminUrl"].as_str().expect("adminUrl").to_string();
        rt().block_on(async {
            let resp = reqwest::Client::new()
                .post(format!("{url_on}/imposters"))
                .json(&inject_imposter)
                .send()
                .await
                .expect("admin POST reachable");
            assert!(
                resp.status().is_success(),
                "allowInjection=true must accept a script imposter over the admin plane, got {}",
                resp.status()
            );
        });
        rift_stop(h_on);

        // Default (allowInjection absent → false) → the same POST is rejected as invalid injection.
        let h_off = rift_start();
        let admin_off = serve_admin(h_off, "{}");
        let url_off = admin_off["adminUrl"]
            .as_str()
            .expect("adminUrl")
            .to_string();
        rt().block_on(async {
            let mut reject = inject_imposter.clone();
            reject["port"] = serde_json::json!(19973);
            let resp = reqwest::Client::new()
                .post(format!("{url_off}/imposters"))
                .json(&reject)
                .send()
                .await
                .expect("admin POST reachable");
            assert_eq!(
                resp.status(),
                400,
                "default (no allowInjection) must reject a script imposter"
            );
            let body: serde_json::Value = resp.json().await.expect("json error body");
            assert_eq!(
                body["errors"][0]["code"], "invalid injection",
                "rejection must be the Mountebank invalid-injection error, got: {body}"
            );
        });
        rift_stop(h_off);
    }
}

// Issue #492: `allowInjection` gates only imposters submitted THROUGH the running admin plane.
// A script imposter supplied via rift_serve_admin's own `config` (startup) is applied directly by
// the manager — the trusted host path — so it is created even with allowInjection off. This pins
// that intentional asymmetry so a future change can't silently start gating the startup path.
#[test]
fn ffi_serve_admin_inline_config_inject_is_ungated() {
    unsafe {
        let h = rift_start();
        let admin = serve_admin(
            h,
            r#"{"allowInjection": false, "config": {"imposters": [
                {"port": 19974, "protocol": "http",
                 "stubs": [{"responses": [{"inject": "function (config) { return { statusCode: 200, body: 'hi' }; }"}]}]}
            ]}}"#,
        );
        let admin_url = admin["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("admin /imposters reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("imposters array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(
                ports.contains(&19974),
                "an inject imposter from the startup `config` must be created even with \
                 allowInjection off (the trusted host path is ungated), got ports: {ports:?}"
            );
        });

        rift_stop(h);
    }
}

// AC2: rift_apply_config returns the reload report field names; failed is [{port,error}]; an
// up-front validation failure returns NULL, sets last_error, and mutates nothing.
#[test]
fn ffi_apply_config_report_fields_and_validation() {
    unsafe {
        let h = rift_start();

        let cfg = cstr(r#"{"imposters":[{"port":19993,"protocol":"http","stubs":[]}]}"#);
        let report = take_json(rift_apply_config(h, cfg.as_ptr()));
        let v: serde_json::Value = serde_json::from_str(&report).expect("report json");
        for k in ["created", "replaced", "stubPatched", "deleted", "failed"] {
            assert!(
                v.get(k).is_some(),
                "apply report has field `{k}` (reload parity)"
            );
        }
        assert!(
            v["created"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_u64() == Some(19993)),
            "19993 reported as created"
        );

        // Duplicate explicit ports parse but fail up-front validation → NULL + last_error, and
        // nothing is mutated (port 19994 is left free to bind afterward).
        let dup = cstr(
            r#"{"imposters":[{"port":19994,"protocol":"http","stubs":[]},{"port":19994,"protocol":"http","stubs":[]}]}"#,
        );
        assert!(
            rift_apply_config(h, dup.as_ptr()).is_null(),
            "invalid config set → NULL"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "validation failure records last_error");
        rift_free(err);
        assert_eq!(
            rift_create_imposter(
                h,
                cstr(r#"{"port":19994,"protocol":"http","stubs":[]}"#).as_ptr()
            ),
            19994,
            "nothing was mutated — 19994 is still free to bind"
        );

        rift_stop(h);
    }
}

// AC3: rift_delete_imposter frees the port (immediate re-create on the same port succeeds);
// deleting an unknown port returns -1.
#[test]
fn ffi_delete_imposter_frees_port() {
    unsafe {
        let h = rift_start();
        let cfg = cstr(r#"{"port":19995,"protocol":"http","stubs":[]}"#);
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19995);

        assert_eq!(rift_delete_imposter(h, 19995), 0, "delete ok");
        assert_eq!(
            rift_create_imposter(h, cfg.as_ptr()),
            19995,
            "port freed — re-create on the same port succeeds"
        );

        assert_eq!(rift_delete_imposter(h, 6553), -1, "unknown port → -1");
        rift_stop(h);
    }
}

// AC4: rift_build_info parses; version == CARGO_PKG_VERSION; features list matches the enabled
// feature set; commit/builtAt are present (string or null).
#[test]
fn ffi_build_info_reports_version_and_features() {
    unsafe {
        let p = rift_build_info();
        assert!(!p.is_null(), "build_info returns a static string");
        // Static string — NOT freed.
        let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
        let v: serde_json::Value = serde_json::from_str(&s).expect("build_info json");

        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            v.get("commit").is_some(),
            "commit field present (string or null)"
        );
        assert!(
            v.get("builtAt").is_some(),
            "builtAt field present (string or null)"
        );

        let features: Vec<&str> = v["features"]
            .as_array()
            .expect("features array")
            .iter()
            .map(|f| f.as_str().expect("feature str"))
            .collect();
        if cfg!(feature = "javascript") {
            assert!(
                features.contains(&"javascript"),
                "javascript feature reported"
            );
        }
        if cfg!(feature = "redis-backend") {
            assert!(
                features.contains(&"redis-backend"),
                "redis-backend feature reported"
            );
        }
    }
}

// AC5: rift_last_error is set on failure, cleared by the next successful call, cleared by reading
// it, and confined to the calling thread.
#[test]
fn ffi_last_error_set_cleared_and_reading_clears() {
    unsafe {
        let h = rift_start();

        // Failure sets it.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        let first = rift_last_error();
        assert!(!first.is_null(), "failure sets last_error");
        rift_free(first);
        // Reading cleared it.
        assert!(rift_last_error().is_null(), "reading last_error clears it");

        // Failure sets it again; a subsequent successful call clears it.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        assert_eq!(
            rift_delete_all(h),
            0,
            "a successful call clears last_error on entry"
        );
        assert!(
            rift_last_error().is_null(),
            "next successful call cleared last_error"
        );

        // Thread-confined: set on this thread, another thread sees none.
        assert!(rift_apply_config(h, cstr("{ not json").as_ptr()).is_null());
        let other = std::thread::spawn(|| rift_last_error().is_null())
            .join()
            .expect("thread");
        assert!(
            other,
            "last_error is thread-local — not visible on another thread"
        );
        let mine = rift_last_error();
        assert!(!mine.is_null(), "this thread's last_error is still set");
        rift_free(mine);

        rift_stop(h);
    }
}

// AC6: an HTTPS imposter created via FFI works without the host pre-installing a rustls provider
// — rift_start installs the ring default provider.
#[test]
fn ffi_https_imposter_without_host_provider() {
    unsafe {
        // NB: no rustls provider is installed by this test; rift_start must do it.
        let h = rift_start();
        let cfg = cstr(
            r#"{"port":19996,"protocol":"https","stubs":[{"responses":[{"is":{"statusCode":200,"body":"secure"}}]}]}"#,
        );
        assert_eq!(
            rift_create_imposter(h, cfg.as_ptr()),
            19996,
            "HTTPS imposter binds (self-signed) — provider is installed"
        );

        let body = rt().block_on(async {
            let client = reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("tls client");
            let r = client
                .get("https://127.0.0.1:19996/anything")
                .send()
                .await
                .expect("https reachable");
            assert_eq!(r.status(), 200);
            r.text().await.expect("body")
        });
        assert_eq!(body, "secure");

        rift_stop(h);
    }
}

// AC7: one admin plane per handle — a second rift_serve_admin fails (NULL) with a last_error.
#[test]
fn ffi_one_admin_plane_per_handle() {
    unsafe {
        let h = rift_start();
        let _ = serve_admin(h, "{}");

        let second = rift_serve_admin(h, cstr("{}").as_ptr());
        assert!(
            second.is_null(),
            "second serve_admin on the same handle fails"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "already-serving failure records last_error");
        rift_free(err);

        rift_stop(h);
    }
}

// AC1 (metricsPort option): serve_admin with metricsPort:0 binds a metrics server and reports its
// assigned port; that port serves /metrics. Without the option, metricsPort is null.
#[test]
fn ffi_serve_admin_with_metrics_port() {
    unsafe {
        let h = rift_start();
        let info = serve_admin(h, r#"{"metricsPort":0}"#);
        let mp = info["metricsPort"].as_u64().expect("metricsPort reported") as u16;
        assert_ne!(mp, 0, "metrics bound to an assigned port");

        let ok = rt().block_on(async {
            reqwest::get(format!("http://127.0.0.1:{mp}/metrics"))
                .await
                .expect("metrics reachable")
                .status()
                == 200
        });
        assert!(ok, "metrics server serves /metrics");
        rift_stop(h);

        // No metricsPort → null in the response.
        let h2 = rift_start();
        let info = serve_admin(h2, "{}");
        assert!(info["metricsPort"].is_null(), "no metricsPort → null");
        rift_stop(h2);
    }
}

/// True once `url`'s host:port refuses TCP connections (listener gone), polled up to ~2s.
async fn admin_refused(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..40 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Err(_)) => return true,
            _ => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    false
}

// AC8b: rift_stop actually shuts the admin/metrics listeners down (both ports refuse afterward).
#[test]
fn ffi_stop_shuts_down_admin_and_metrics() {
    unsafe {
        let h = rift_start();
        let info = serve_admin(h, r#"{"metricsPort":0}"#);
        let admin_port = info["adminPort"].as_u64().expect("adminPort") as u16;
        let metrics_port = info["metricsPort"].as_u64().expect("metricsPort") as u16;
        rt().block_on(async {
            // Both are up while serving.
            assert_eq!(
                reqwest::get(format!("http://127.0.0.1:{admin_port}/health"))
                    .await
                    .expect("admin up")
                    .status(),
                200
            );
        });

        rift_stop(h);

        rt().block_on(async {
            assert!(
                admin_refused(admin_port).await,
                "admin port closed by rift_stop"
            );
            assert!(
                admin_refused(metrics_port).await,
                "metrics port closed by rift_stop"
            );
        });
    }
}

// AC1 (apiKey option) + the security contract: a string apiKey gates the admin control plane; a
// wrong-typed apiKey is a hard error (NULL + last_error), never a silent unauthenticated plane.
#[test]
fn ffi_serve_admin_apikey_gates_and_rejects_wrong_type() {
    unsafe {
        // Wrong-type apiKey must fail loudly, not silently disable auth.
        let h = rift_start();
        assert!(
            rift_serve_admin(h, cstr(r#"{"apiKey":12345}"#).as_ptr()).is_null(),
            "non-string apiKey is rejected, not silently unauthenticated"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "wrong-type apiKey records last_error");
        rift_free(err);

        // A string apiKey gates the admin API.
        let info = serve_admin(h, r#"{"apiKey":"secret"}"#);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();
        rt().block_on(async {
            let unauthed = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable");
            assert_eq!(unauthed.status(), 401, "admin requires the apiKey");

            let authed = reqwest::Client::new()
                .get(format!("{admin_url}/imposters"))
                .header("authorization", "secret")
                .send()
                .await
                .expect("reachable");
            assert_eq!(authed.status(), 200, "correct apiKey is accepted");
        });
        rift_stop(h);
    }
}

// AC1 (configFile option): serve_admin loads imposters from a config file and wires it as the
// reload source; the loaded imposter is visible over the admin API.
#[test]
fn ffi_serve_admin_loads_config_file() {
    unsafe {
        let path = std::env::temp_dir().join(format!("rift_ffi_cfg_{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19998,"protocol":"http","stubs":[]}]}"#,
        )
        .expect("write config file");

        let h = rift_start();
        let opts = format!(
            r#"{{"configFile":{}}}"#,
            serde_json::json!(path.to_str().unwrap())
        );
        let info = serve_admin(h, &opts);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(
                ports.contains(&19998),
                "configFile imposter loaded and visible"
            );
        });
        rift_stop(h);
        let _ = std::fs::remove_file(&path);
    }
}

// AC2 (per-port failure shape): a config whose port is already occupied applies with that port in
// `failed` as {port, error} — nothing up-front-invalid, so the report (not NULL) comes back.
#[test]
fn ffi_apply_config_reports_per_port_failure() {
    unsafe {
        // Occupy 0.0.0.0:<port> — the wildcard address imposters bind — so the manager's bind for
        // that port deterministically collides during apply.
        let occupied = std::net::TcpListener::bind("0.0.0.0:0").expect("occupy port");
        let busy = occupied.local_addr().expect("addr").port();

        let h = rift_start();
        let cfg = cstr(&format!(
            r#"{{"imposters":[{{"port":{busy},"protocol":"http","stubs":[]}}]}}"#
        ));
        let report = take_json(rift_apply_config(h, cfg.as_ptr()));
        let v: serde_json::Value = serde_json::from_str(&report).expect("report json");

        let failed = v["failed"].as_array().expect("failed array");
        assert_eq!(failed.len(), 1, "the occupied port failed to bind");
        assert_eq!(
            failed[0]["port"].as_u64(),
            Some(u64::from(busy)),
            "failed entry carries the port"
        );
        assert!(
            failed[0]["error"].as_str().is_some_and(|s| !s.is_empty()),
            "failed entry carries a non-empty error string"
        );
        rift_stop(h);
    }
}

// AC1 (#344): build.rs stamps the commit, so rift_build_info's `commit` equals `git rev-parse
// HEAD` in a git checkout (and `builtAt` is now a real timestamp, not null).
#[test]
fn ffi_build_info_commit_matches_git_head() {
    unsafe {
        let s = CStr::from_ptr(rift_build_info())
            .to_str()
            .expect("utf8")
            .to_owned();
        let v: serde_json::Value = serde_json::from_str(&s).expect("build_info json");

        // Mirror build.rs's own resolution: an explicit RIFT_COMMIT override wins (as it would in
        // CI), else `git rev-parse HEAD`. build.rs reruns on HEAD moves, so this stays equal.
        let expected = expected_commit();
        assert_eq!(
            v["commit"].as_str(),
            Some(expected.as_str()),
            "build.rs stamps RIFT_COMMIT with the current HEAD (or the env override)"
        );
        assert!(
            v["builtAt"].as_str().is_some(),
            "build.rs stamps RIFT_BUILT_AT"
        );
    }
}

// Issue #350: a configFile whose N-th imposter can't bind must NOT leave partial state behind a
// NULL return. Routed through apply_config, serve_admin succeeds (non-NULL), loads the bindable
// imposters, and reports the unbindable one as a per-port failure instead of a half-applied loop.
#[test]
fn ffi_serve_admin_config_file_partial_failure_does_not_leak() {
    unsafe {
        // Occupy the wildcard port an imposter would bind, so one config in the file cannot bind.
        let occupied = std::net::TcpListener::bind("0.0.0.0:0").expect("occupy port");
        let busy = occupied.local_addr().expect("addr").port();

        let path =
            std::env::temp_dir().join(format!("rift_ffi_partial_{}.json", std::process::id()));
        std::fs::write(
            &path,
            format!(
                r#"{{"imposters":[{{"port":19970,"protocol":"http","stubs":[]}},{{"port":{busy},"protocol":"http","stubs":[]}}]}}"#
            ),
        )
        .expect("write config file");

        let h = rift_start();
        let opts = format!(
            r#"{{"configFile":{}}}"#,
            serde_json::json!(path.to_str().unwrap())
        );
        // serve_admin() asserts non-NULL — the crux of the fix (the old create-loop returned NULL
        // here after already creating 19970).
        let info = serve_admin(h, &opts);
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();

        rt().block_on(async {
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable")
                .json()
                .await
                .expect("json");
            let ports: Vec<u64> = imps["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .filter_map(|i| i["port"].as_u64())
                .collect();
            assert!(ports.contains(&19970), "the bindable imposter loaded");
            assert!(
                !ports.contains(&u64::from(busy)),
                "the occupied port did not bind and was not partially left behind"
            );
        });

        rift_stop(h);
        let _ = std::fs::remove_file(&path);
    }
}

// Issue #350 (second-order): a retry after a partial serve_admin failure is idempotent. The first
// attempt applies the configFile imposter, then fails at the metrics bind (NULL, admin slot unset).
// The old create-loop's retry would hit PortInUse re-creating that imposter; apply_config's
// reconcile treats the unchanged imposter as a no-op, so the retry succeeds with no duplicate.
#[test]
fn ffi_serve_admin_retry_after_partial_failure_is_idempotent() {
    unsafe {
        // Occupy the loopback metrics port (serve_admin binds metrics on 127.0.0.1 by default) so
        // the FIRST serve_admin fails at the metrics bind, AFTER the configFile imposter is applied.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("occupy port");
        let busy_metrics = occupied.local_addr().expect("addr").port();

        let path = std::env::temp_dir().join(format!("rift_ffi_retry_{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"imposters":[{"port":19971,"protocol":"http","stubs":[]}]}"#,
        )
        .expect("write config file");
        let cf = serde_json::json!(path.to_str().unwrap());

        let h = rift_start();

        // First attempt: configFile applies (creates 19971), then the metrics bind fails → NULL.
        let first = rift_serve_admin(
            h,
            cstr(&format!(
                r#"{{"configFile":{cf},"metricsPort":{busy_metrics}}}"#
            ))
            .as_ptr(),
        );
        assert!(
            first.is_null(),
            "first serve_admin fails at the occupied metrics port"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "the failure records last_error");
        rift_free(err);

        drop(occupied);

        // Retry with an ephemeral metrics port: apply_config sees 19971 unchanged (reconcile no-op),
        // so this succeeds instead of hitting PortInUse.
        let info = serve_admin(h, &format!(r#"{{"configFile":{cf},"metricsPort":0}}"#));
        let admin_url = info["adminUrl"].as_str().expect("adminUrl").to_string();
        rt().block_on(async {
            let imps: serde_json::Value = reqwest::get(format!("{admin_url}/imposters"))
                .await
                .expect("reachable")
                .json()
                .await
                .expect("json");
            let count = imps["imposters"]
                .as_array()
                .expect("array")
                .iter()
                .filter(|i| i["port"].as_u64() == Some(19971))
                .count();
            assert_eq!(
                count, 1,
                "imposter present exactly once after retry — no duplicate/conflict"
            );
        });

        rift_stop(h);
        let _ = std::fs::remove_file(&path);
    }
}

/// Issue #411: the admin long tail — scenario/flow-state and correlated spaces — over direct
/// C-ABI, with the same wire fidelity as the admin-HTTP handlers.
#[test]
fn ffi_admin_plane_round_trip() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null());
        let config = cstr(
            r#"{ "port": 19991, "protocol": "http", "recordRequests": true,
                 "_rift": { "flowState": { "backend": "inmemory" } }, "stubs": [] }"#,
        );
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19991);

        // --- flow state: put -> get (JSON {flowId,key,value}) -> delete -> get(null) ---
        let flow = cstr("flow-1");
        let key = cstr("state");
        let val = cstr(r#""paid""#);
        assert_eq!(
            rift_flow_state_put(h, port, flow.as_ptr(), key.as_ptr(), val.as_ptr()),
            0
        );
        let got: serde_json::Value = serde_json::from_str(&take_json(rift_flow_state_get(
            h,
            port,
            flow.as_ptr(),
            key.as_ptr(),
        )))
        .unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["flowId"], "flow-1");
        assert_eq!(got["key"], "state");
        assert_eq!(got["value"], "paid");
        assert_eq!(
            rift_flow_state_delete(h, port, flow.as_ptr(), key.as_ptr()),
            0
        );
        let after_del: serde_json::Value = serde_json::from_str(&take_json(rift_flow_state_get(
            h,
            port,
            flow.as_ptr(),
            key.as_ptr(),
        )))
        .unwrap();
        assert_eq!(
            after_del["found"], false,
            "get after delete -> found:false (absent), not a null/error"
        );
        assert!(after_del["value"].is_null());

        // --- correlated space stubs: add -> list ({space,stubs}) -> delete -> list(empty) ---
        let space = cstr("space-a");
        let stub = cstr(
            r#"{ "predicates": [{ "equals": { "path": "/x" } }], "responses": [{ "is": { "statusCode": 204 } }] }"#,
        );
        assert_eq!(
            rift_space_add_stub(h, port, space.as_ptr(), stub.as_ptr()),
            0
        );
        let listed: serde_json::Value =
            serde_json::from_str(&take_json(rift_space_list_stubs(h, port, space.as_ptr())))
                .unwrap();
        assert_eq!(listed["space"], "space-a");
        assert_eq!(listed["stubs"].as_array().unwrap().len(), 1);
        assert_eq!(rift_space_delete(h, port, space.as_ptr()), 0);
        let after: serde_json::Value =
            serde_json::from_str(&take_json(rift_space_list_stubs(h, port, space.as_ptr())))
                .unwrap();
        assert_eq!(
            after["stubs"].as_array().unwrap().len(),
            0,
            "teardown removed the space's stubs"
        );

        // --- header-filtered recorded (issue #201): default flow-id source is the port ---
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = reqwest::get("http://127.0.0.1:19991/ping").await;
        });
        let matching = cstr("19991");
        let recorded: serde_json::Value =
            serde_json::from_str(&take_json(rift_space_recorded(h, port, matching.as_ptr())))
                .unwrap();
        assert_eq!(
            recorded.as_array().unwrap().len(),
            1,
            "recorded filtered to the matching flow-id"
        );
        let other = cstr("other-flow");
        let none: serde_json::Value =
            serde_json::from_str(&take_json(rift_space_recorded(h, port, other.as_ptr()))).unwrap();
        assert_eq!(
            none.as_array().unwrap().len(),
            0,
            "a non-matching flow-id yields no recorded requests"
        );

        assert_eq!(rift_delete_all(h), 0);
        rift_stop(h);
    }
}

/// Issue #411: null-handle and unknown-port error paths map to the crate's sentinels.
#[test]
fn ffi_admin_plane_error_paths() {
    unsafe {
        let flow = cstr("f");
        let key = cstr("k");
        let one = cstr("1");
        // Null handle -> the documented sentinel for each return shape...
        assert!(
            rift_flow_state_get(std::ptr::null_mut(), 1, flow.as_ptr(), key.as_ptr()).is_null()
        );
        // ...and the failure records `last_error` (reading it clears it).
        let err = rift_last_error();
        assert!(!err.is_null(), "flow_state_get failure records last_error");
        rift_free(err);

        assert_eq!(
            rift_flow_state_put(
                std::ptr::null_mut(),
                1,
                flow.as_ptr(),
                key.as_ptr(),
                one.as_ptr()
            ),
            -1
        );
        assert_eq!(
            rift_flow_state_delete(std::ptr::null_mut(), 1, flow.as_ptr(), key.as_ptr()),
            -1
        );
        assert_eq!(
            rift_space_add_stub(std::ptr::null_mut(), 1, flow.as_ptr(), key.as_ptr()),
            -1
        );
        assert!(rift_space_list_stubs(std::ptr::null_mut(), 1, flow.as_ptr()).is_null());
        assert_eq!(
            rift_space_delete(std::ptr::null_mut(), 1, flow.as_ptr()),
            -1
        );
        assert!(rift_space_recorded(std::ptr::null_mut(), 1, flow.as_ptr()).is_null());

        // Live handle, unknown port -> same sentinels for all seven.
        let h = rift_start();
        assert!(rift_flow_state_get(h, 65000, flow.as_ptr(), key.as_ptr()).is_null());
        assert_eq!(
            rift_flow_state_put(h, 65000, flow.as_ptr(), key.as_ptr(), one.as_ptr()),
            -1
        );
        assert_eq!(
            rift_flow_state_delete(h, 65000, flow.as_ptr(), key.as_ptr()),
            -1
        );
        assert_eq!(
            rift_space_add_stub(h, 65000, flow.as_ptr(), cstr("{}").as_ptr()),
            -1
        );
        assert_eq!(rift_space_delete(h, 65000, flow.as_ptr()), -1);
        assert!(rift_space_list_stubs(h, 65000, flow.as_ptr()).is_null());
        assert!(rift_space_recorded(h, 65000, flow.as_ptr()).is_null());
        // The last failure also recorded a message (AC3).
        let err2 = rift_last_error();
        assert!(!err2.is_null(), "unknown-port failure records last_error");
        rift_free(err2);
        rift_stop(h);
    }
}

/// Issue #415: `rift_flow_state_get` gives an unambiguous "not found" signal — an absent key is a
/// first-class value (`{"found":false}`), distinct from a genuine error (null pointer). Covers all
/// three outcomes without parsing `rift_last_error`.
#[test]
fn ffi_flow_state_get_absent_vs_error() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null());
        let config = cstr(
            r#"{ "port": 19994, "protocol": "http",
                 "_rift": { "flowState": { "backend": "inmemory" } }, "stubs": [] }"#,
        );
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19994);

        let flow = cstr("flow-x");
        let present = cstr("present");
        let absent = cstr("absent");
        let val = cstr(r#""ready""#);

        // present key -> found:true carrying the value.
        assert_eq!(
            rift_flow_state_put(h, port, flow.as_ptr(), present.as_ptr(), val.as_ptr()),
            0
        );
        let hit: serde_json::Value = serde_json::from_str(&take_json(rift_flow_state_get(
            h,
            port,
            flow.as_ptr(),
            present.as_ptr(),
        )))
        .unwrap();
        assert_eq!(hit["found"], true);
        assert_eq!(hit["value"], "ready");

        // a *stored* JSON null is found:true with value null — distinct from an absent key. This is
        // the exact ambiguity the `found` field exists to resolve.
        let null_key = cstr("null-value");
        assert_eq!(
            rift_flow_state_put(
                h,
                port,
                flow.as_ptr(),
                null_key.as_ptr(),
                cstr("null").as_ptr()
            ),
            0
        );
        let stored_null: serde_json::Value = serde_json::from_str(&take_json(rift_flow_state_get(
            h,
            port,
            flow.as_ptr(),
            null_key.as_ptr(),
        )))
        .unwrap();
        assert_eq!(
            stored_null["found"], true,
            "a stored null is present, not absent"
        );
        assert!(stored_null["value"].is_null());

        // absent key -> found:false, value null, and NOT an error (no null pointer, last_error clear).
        let ptr = rift_flow_state_get(h, port, flow.as_ptr(), absent.as_ptr());
        assert!(!ptr.is_null(), "an absent key is a value, not a null/error");
        let miss: serde_json::Value = serde_json::from_str(&take_json(ptr)).unwrap();
        assert_eq!(miss["found"], false);
        assert!(miss["value"].is_null());
        assert!(
            rift_last_error().is_null(),
            "an absent key must not record last_error"
        );

        // bad port -> genuine error -> null pointer, and last_error recorded.
        assert!(
            rift_flow_state_get(h, 65001, flow.as_ptr(), absent.as_ptr()).is_null(),
            "an unknown port is a genuine error -> null"
        );
        let err = rift_last_error();
        assert!(!err.is_null(), "the error path records last_error");
        rift_free(err);

        assert_eq!(rift_delete_all(h), 0);
        rift_stop(h);
    }
}

/// Issue #410: the intercept listener + control plane entirely over FFI — start the listener,
/// add serve + forward rules, fetch the CA, and drive an HTTPS client (trusting only that CA)
/// through the intercept port to both a served stub and a forwarded FFI imposter.
#[test]
fn ffi_intercept_serve_and_forward() {
    unsafe {
        let h = rift_start();

        // An FFI-created imposter is the target of the `forward` rule.
        let upstream = cstr(
            r#"{ "port": 19993, "protocol": "http",
                 "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "forwarded" } }] }] }"#,
        );
        assert_eq!(rift_create_imposter(h, upstream.as_ptr()), 19993);

        // Start the intercept listener on an OS-assigned port; learn interceptPort.
        let started = take_json(rift_start_intercept(h, cstr(r#"{"port":0}"#).as_ptr()));
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let intercept_port = started["interceptPort"].as_u64().expect("interceptPort") as u16;
        assert!(intercept_port > 0);

        // Fetch the CA (needed to trust the minted leaves).
        let ca_pem = take_json(rift_intercept_ca_pem(h));
        assert!(ca_pem.starts_with("-----BEGIN CERTIFICATE-----"));

        // Add a serve rule and a forward rule over FFI (batch).
        let rules = cstr(
            r#"[ { "host": "cdn.example.com", "action": { "serve": { "statusCode": 418, "body": "served" } } },
                 { "host": "fwd.example.com", "action": { "forward": { "port": 19993 } } } ]"#,
        );
        assert_eq!(rift_intercept_add_rules(h, rules.as_ptr()), 0);

        // list reflects the added rules (Read completes the CRUD surface, all over FFI).
        let listed: serde_json::Value =
            serde_json::from_str(&take_json(rift_intercept_list_rules(h))).unwrap();
        assert_eq!(
            listed.as_array().unwrap().len(),
            2,
            "both rules are listed over FFI"
        );

        // Drive HTTPS through the intercept port with a client trusting only the intercept CA.
        rt().block_on(async {
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::https(format!("http://127.0.0.1:{intercept_port}")).unwrap())
                .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
                .build()
                .unwrap();

            let served = client
                .get("https://cdn.example.com/x")
                .send()
                .await
                .expect("served");
            assert_eq!(served.status(), 418);
            assert_eq!(served.text().await.unwrap(), "served");

            let forwarded = client
                .get("https://fwd.example.com/y")
                .send()
                .await
                .expect("forwarded");
            assert_eq!(forwarded.status(), 200);
            assert_eq!(forwarded.text().await.unwrap(), "forwarded");
        });

        rift_stop(h);
    }
}

/// Export a truststore over FFI to a temp path, assert success, and return the written bytes.
unsafe fn export_truststore_bytes(
    h: *mut RiftHandle,
    format: &str,
    password: *const c_char,
) -> Vec<u8> {
    unsafe {
        let path_buf =
            std::env::temp_dir().join(format!("rift410-{}-{format}.store", std::process::id()));
        let path = cstr(path_buf.to_str().unwrap());
        let fmt = cstr(format);
        assert_eq!(
            rift_intercept_export_truststore(h, fmt.as_ptr(), password, path.as_ptr()),
            0,
            "export {format} succeeds"
        );
        let bytes = std::fs::read(&path_buf).unwrap();
        let _ = std::fs::remove_file(&path_buf);
        bytes
    }
}

/// Issue #410 (AC3): truststore export writes structurally-valid PKCS#12 and JKS for the CA,
/// with the default and a caller-supplied password; an unknown format is rejected.
#[test]
fn ffi_intercept_truststore_export() {
    unsafe {
        let h = rift_start();
        take_json(rift_start_intercept(h, std::ptr::null()));

        // PKCS#12 (default password) is DER — starts with a SEQUENCE tag.
        let p12 = export_truststore_bytes(h, "pkcs12", std::ptr::null());
        assert_eq!(
            p12.first(),
            Some(&0x30),
            "PKCS#12 begins with a DER SEQUENCE"
        );

        // JKS (a caller-supplied password) starts with the JKS magic 0xFEEDFEED.
        let pw = cstr("hunter2");
        let jks = export_truststore_bytes(h, "jks", pw.as_ptr());
        assert_eq!(jks[..4], [0xFE, 0xED, 0xFE, 0xED], "JKS magic 0xFEEDFEED");

        // An unknown format is a clear error, not a silent write.
        let bad = cstr("pem");
        let out = cstr(
            std::env::temp_dir()
                .join("rift410-unused")
                .to_str()
                .unwrap(),
        );
        assert_eq!(
            rift_intercept_export_truststore(h, bad.as_ptr(), std::ptr::null(), out.as_ptr()),
            -1,
            "unknown truststore format -> -1"
        );

        assert_eq!(rift_intercept_clear_rules(h), 0);
        rift_stop(h);
    }
}

/// Issue #410 (AC4): rift_stop shuts the intercept listener down and frees its port (no orphan),
/// and the start response's interceptUrl matches the bound port (AC1).
#[test]
fn ffi_stop_shuts_down_intercept() {
    unsafe {
        let h = rift_start();
        let started: serde_json::Value =
            serde_json::from_str(&take_json(rift_start_intercept(h, std::ptr::null()))).unwrap();
        let port = started["interceptPort"].as_u64().unwrap() as u16;
        assert_eq!(
            started["interceptUrl"].as_str().unwrap(),
            format!("http://127.0.0.1:{port}")
        );
        rift_stop(h);
        assert!(
            rt().block_on(admin_refused(port)),
            "intercept port is freed after rift_stop (no orphaned listener)"
        );
    }
}

/// Issue #425: the start response's interceptUrl reflects the ACTUAL bound host, not a hardcoded
/// 127.0.0.1. A non-loopback bind (0.0.0.0) must surface as the bound address; the loopback default
/// is unchanged.
#[test]
fn ffi_intercept_url_reflects_bind_host() {
    unsafe {
        // Non-loopback bind (0.0.0.0 wildcard, OS-assigned port) -> URL reflects the bound host.
        let h = rift_start();
        let started: serde_json::Value = serde_json::from_str(&take_json(rift_start_intercept(
            h,
            cstr(r#"{"host":"0.0.0.0","port":0}"#).as_ptr(),
        )))
        .unwrap();
        let port = started["interceptPort"].as_u64().expect("interceptPort");
        assert!(port > 0, "OS-assigned port is surfaced");
        assert_eq!(
            started["interceptUrl"].as_str().expect("interceptUrl"),
            format!("http://0.0.0.0:{port}"),
            "interceptUrl reflects the bound host, not a hardcoded 127.0.0.1"
        );
        rift_stop(h);

        // Loopback default (AC2) is unchanged.
        let h2 = rift_start();
        let d: serde_json::Value =
            serde_json::from_str(&take_json(rift_start_intercept(h2, cstr("{}").as_ptr())))
                .unwrap();
        let p2 = d["interceptPort"].as_u64().expect("interceptPort");
        assert_eq!(
            d["interceptUrl"].as_str().unwrap(),
            format!("http://127.0.0.1:{p2}"),
            "loopback default unchanged"
        );
        rift_stop(h2);
    }
}

/// Issue #410: opt-in — a handle that never started intercept rejects control calls (not started),
/// and the data plane is unaffected.
#[test]
fn ffi_intercept_optin_not_started() {
    unsafe {
        let h = rift_start();
        assert!(
            rift_intercept_ca_pem(h).is_null(),
            "ca_pem before start -> null"
        );
        assert!(
            rift_intercept_list_rules(h).is_null(),
            "list before start -> null"
        );
        assert_eq!(
            rift_intercept_add_rules(h, cstr("[]").as_ptr()),
            -1,
            "add_rules before start -> -1"
        );
        assert_eq!(rift_intercept_clear_rules(h), -1);
        let out = cstr("/tmp/rift410-never");
        assert_eq!(
            rift_intercept_export_truststore(
                h,
                cstr("jks").as_ptr(),
                std::ptr::null(),
                out.as_ptr()
            ),
            -1,
            "export before start -> -1"
        );
        let last = rift_last_error();
        assert!(!last.is_null(), "not-started failure records last_error");
        rift_free(last);
        // Data plane still works with intercept never started.
        let cfg = cstr(r#"{ "port": 19994, "protocol": "http", "stubs": [] }"#);
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19994);
        rift_stop(h);
    }
}

/// Mint a committed CA (cert + key PEM) to two temp files and return their paths, mirroring the
/// certificate shape `CertificateAuthority::generate()` produces so loaded leaves validate.
fn write_committed_ca(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    let key = KeyPair::generate().expect("generate CA key");
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
        .distinguished_name
        .push(DnType::CommonName, "Rift Test Committed CA");
    let cert = params.self_signed(&key).expect("self-sign CA");
    let dir = std::env::temp_dir();
    let cert_path = dir.join(format!("rift429-{}-{tag}-ca.pem", std::process::id()));
    let key_path = dir.join(format!("rift429-{}-{tag}-ca.key", std::process::id()));
    std::fs::write(&cert_path, cert.pem()).expect("write CA cert");
    std::fs::write(&key_path, key.serialize_pem()).expect("write CA key");
    (cert_path, key_path)
}

/// Issue #429 (AC1/AC4): two `rift_start_intercept` instances started with the SAME committed CA
/// present mutually-trusted leaves — a truststore holding instance A's exported CA validates the
/// TLS instance B intercepts. Both instances expose the committed CA verbatim (loaded, not
/// regenerated).
#[test]
fn ffi_intercept_reuses_committed_ca() {
    unsafe {
        let (cert_path, key_path) = write_committed_ca("reuse");
        let committed = std::fs::read_to_string(&cert_path).unwrap();

        let opts = serde_json::json!({
            "port": 0,
            "caCertPath": cert_path.to_str().unwrap(),
            "caKeyPath": key_path.to_str().unwrap(),
        })
        .to_string();

        // Instance A and instance B both load the same committed CA.
        let ha = rift_start();
        take_json(rift_start_intercept(ha, cstr(&opts).as_ptr()));
        let hb = rift_start();
        let started_b: serde_json::Value =
            serde_json::from_str(&take_json(rift_start_intercept(hb, cstr(&opts).as_ptr())))
                .unwrap();
        let port_b = started_b["interceptPort"].as_u64().expect("interceptPort") as u16;

        // Both expose the committed CA verbatim — loaded, not regenerated.
        let ca_a = take_json(rift_intercept_ca_pem(ha));
        let ca_b = take_json(rift_intercept_ca_pem(hb));
        assert_eq!(ca_a, ca_b, "both instances expose the same CA");
        assert_eq!(
            ca_a.trim(),
            committed.trim(),
            "instance A exposes the committed CA, not a fresh one"
        );

        // Instance B intercepts and serves a host.
        let rules = cstr(
            r#"[{ "host": "reuse.example.com",
                  "action": { "serve": { "statusCode": 200, "body": "served-by-b" } } }]"#,
        );
        assert_eq!(rift_intercept_add_rules(hb, rules.as_ptr()), 0);

        // A client trusting ONLY instance A's exported CA validates instance B's intercepted TLS —
        // proving the committed CA is a shared trust anchor across independent instances.
        rt().block_on(async {
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::https(format!("http://127.0.0.1:{port_b}")).unwrap())
                .add_root_certificate(reqwest::Certificate::from_pem(ca_a.as_bytes()).unwrap())
                .build()
                .unwrap();
            let resp = client
                .get("https://reuse.example.com/x")
                .send()
                .await
                .expect("A's truststore validates B's intercepted leaf");
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "served-by-b");
        });

        rift_stop(ha);
        rift_stop(hb);
        let _ = std::fs::remove_file(&cert_path);
        let _ = std::fs::remove_file(&key_path);
    }
}

/// Read and free `rift_last_error`, asserting it is present.
unsafe fn take_last_error() -> String {
    unsafe {
        let p = rift_last_error();
        assert!(!p.is_null(), "expected a recorded last_error");
        let s = CStr::from_ptr(p).to_str().expect("utf8").to_owned();
        rift_free(p);
        s
    }
}

/// Issue #429 (AC2): a half-configured CA pair is rejected — in either direction — with a clear
/// both-or-neither error rather than silently generating an ephemeral CA.
#[test]
fn ffi_intercept_ca_both_or_neither() {
    unsafe {
        // caCertPath without caKeyPath.
        let h = rift_start();
        let cert_only = serde_json::json!({ "port": 0, "caCertPath": "/nonexistent/ca.pem" });
        assert!(
            rift_start_intercept(h, cstr(&cert_only.to_string()).as_ptr()).is_null(),
            "caCertPath alone -> rejected, not a silent ephemeral CA"
        );
        assert!(
            take_last_error().contains("provided together"),
            "the error names the both-or-neither rule"
        );

        // caKeyPath without caCertPath (the mirror direction).
        let key_only = serde_json::json!({ "port": 0, "caKeyPath": "/nonexistent/ca.key" });
        assert!(
            rift_start_intercept(h, cstr(&key_only.to_string()).as_ptr()).is_null(),
            "caKeyPath alone -> rejected"
        );
        assert!(take_last_error().contains("provided together"));

        rift_stop(h);
    }
}

/// Issue #429: a misspelled CA option key is a hard error (deny_unknown_fields), never a silent
/// fallback to a fresh ephemeral CA that would quietly defeat the caller's intended CA reuse.
#[test]
fn ffi_intercept_rejects_unknown_ca_option() {
    unsafe {
        let h = rift_start();
        let typo = serde_json::json!({ "port": 0, "caCertpath": "/some/ca.pem" });
        assert!(
            rift_start_intercept(h, cstr(&typo.to_string()).as_ptr()).is_null(),
            "a typo'd CA option key is rejected, not silently ignored"
        );
        assert!(!take_last_error().is_empty(), "records why it was rejected");
        rift_stop(h);
    }
}

// ===========================================================================
// Issue #491: admin long-tail symbols over the direct C-ABI — list/get imposters,
// stub surgery (add/get/update/delete by index or id), clear recorded / proxy
// recordings, enable/disable, and scenario list/set-state/reset. Each mirrors an
// admin route by calling the same ImposterManager/Imposter method.
// ===========================================================================

#[test]
fn ffi_admin_longtail_round_trip() {
    unsafe {
        let h = rift_start();
        assert!(!h.is_null());

        let config = cstr(
            r#"{ "port": 19960, "protocol": "http", "recordRequests": true,
                 "stubs": [ { "id": "s1", "scenarioName": "order",
                              "requiredScenarioState": "Started",
                              "predicates": [{ "equals": { "path": "/a" } }],
                              "responses": [{ "is": { "statusCode": 200, "body": "a" } }] } ] }"#,
        );
        let port = rift_create_imposter(h, config.as_ptr());
        assert_eq!(port, 19960);

        // 1. list imposters (summary form) — no _links, just the domain projection
        let list = take_json(rift_list_imposters(h, std::ptr::null()));
        let lv: serde_json::Value = serde_json::from_str(&list).unwrap();
        assert_eq!(lv["imposters"][0]["port"], 19960);
        assert_eq!(lv["imposters"][0]["enabled"], true);
        assert!(lv["imposters"][0]["numberOfRequests"].is_number());

        // 1b. list imposters (replayable) — full configs carrying stubs
        let opts = cstr(r#"{"replayable":true}"#);
        let listr = take_json(rift_list_imposters(h, opts.as_ptr()));
        let lrv: serde_json::Value = serde_json::from_str(&listr).unwrap();
        assert!(lrv["imposters"][0]["stubs"].is_array());

        // 2. get one imposter (detail) + replayable projection
        let det = take_json(rift_get_imposter(h, port, std::ptr::null()));
        let dv: serde_json::Value = serde_json::from_str(&det).unwrap();
        assert_eq!(dv["port"], 19960);
        assert_eq!(dv["recordRequests"], true);
        assert_eq!(dv["stubs"].as_array().unwrap().len(), 1);
        let repl_opts = cstr(r#"{"replayable":true}"#);
        let rep = take_json(rift_get_imposter(h, port, repl_opts.as_ptr()));
        let rv: serde_json::Value = serde_json::from_str(&rep).unwrap();
        assert_eq!(rv["port"], 19960);
        assert!(rv["stubs"].is_array());

        // 3. add one stub (append, index -1)
        let new_stub = cstr(
            r#"{ "id": "s2", "predicates": [{ "equals": { "path": "/b" } }],
                 "responses": [{ "is": { "statusCode": 201, "body": "b" } }] }"#,
        );
        assert_eq!(rift_add_stub(h, port, new_stub.as_ptr(), -1), 0);
        let det2 = take_json(rift_get_imposter(h, port, std::ptr::null()));
        let dv2: serde_json::Value = serde_json::from_str(&det2).unwrap();
        assert_eq!(dv2["stubs"].as_array().unwrap().len(), 2, "stub appended");

        // 4. get stub by index and by id
        let by_idx = take_json(rift_get_stub(h, port, cstr(r#"{"index":1}"#).as_ptr()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&by_idx).unwrap()["id"],
            "s2"
        );
        let by_id = take_json(rift_get_stub(h, port, cstr(r#"{"id":"s1"}"#).as_ptr()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&by_id).unwrap()["id"],
            "s1"
        );

        // 5. update stub (by id)
        let updated = cstr(
            r#"{ "id": "s2", "predicates": [{ "equals": { "path": "/b2" } }],
                 "responses": [{ "is": { "statusCode": 202, "body": "b2" } }] }"#,
        );
        assert_eq!(
            rift_update_stub(h, port, cstr(r#"{"id":"s2"}"#).as_ptr(), updated.as_ptr()),
            0
        );

        // 6. delete stub (by index) → back to one stub
        assert_eq!(
            rift_delete_stub(h, port, cstr(r#"{"index":1}"#).as_ptr()),
            0
        );
        let det3 = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&det3).unwrap()["stubs"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // 7. record a request, then clear recorded
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            reqwest::get("http://127.0.0.1:19960/a").await.expect("get");
        });
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&take_json(rift_recorded(h, port)))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(rift_clear_recorded(h, port), 0);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&take_json(rift_recorded(h, port)))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0,
            "recorded cleared"
        );

        // 8. clear proxy recordings (no-op here, but must succeed)
        assert_eq!(rift_clear_proxy_recordings(h, port), 0);

        // 9. enable/disable
        assert_eq!(rift_set_imposter_enabled(h, port, 0), 0);
        let disabled = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&disabled).unwrap()["enabled"],
            false
        );
        assert_eq!(rift_set_imposter_enabled(h, port, 1), 0);

        // 10-12. scenarios: set state, list, reset
        assert_eq!(
            rift_set_scenario_state(
                h,
                port,
                cstr("order").as_ptr(),
                cstr(r#"{"state":"paid"}"#).as_ptr()
            ),
            0
        );
        let scen = take_json(rift_scenarios(h, port, std::ptr::null()));
        let sv: serde_json::Value = serde_json::from_str(&scen).unwrap();
        let order = sv["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "order")
            .expect("order scenario present");
        assert_eq!(order["state"], "paid");
        assert_eq!(rift_reset_scenarios(h, port, std::ptr::null()), 0);
        let scen2 = take_json(rift_scenarios(h, port, std::ptr::null()));
        let sv2: serde_json::Value = serde_json::from_str(&scen2).unwrap();
        let order2 = sv2["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "order")
            .expect("order scenario present");
        assert_eq!(order2["state"], "Started", "reset returns to initial state");

        rift_stop(h);
    }
}

#[test]
fn ffi_admin_longtail_error_paths() {
    unsafe {
        // null handle → sentinels + last_error
        assert!(rift_list_imposters(std::ptr::null_mut(), std::ptr::null()).is_null());
        assert!(rift_get_imposter(std::ptr::null_mut(), 1, std::ptr::null()).is_null());
        assert_eq!(
            rift_add_stub(std::ptr::null_mut(), 1, std::ptr::null(), -1),
            -1
        );
        assert_eq!(rift_clear_recorded(std::ptr::null_mut(), 1), -1);
        assert_eq!(rift_set_imposter_enabled(std::ptr::null_mut(), 1, 1), -1);

        let h = rift_start();
        // unknown port → sentinels
        assert!(rift_get_imposter(h, 12345, std::ptr::null()).is_null());
        assert_eq!(rift_clear_recorded(h, 12345), -1);
        assert_eq!(rift_set_imposter_enabled(h, 12345, 1), -1);

        // create an imposter with no stubs for ref/JSON error paths
        let cfg = cstr(r#"{ "port": 19961, "protocol": "http", "stubs": [] }"#);
        assert_eq!(rift_create_imposter(h, cfg.as_ptr()), 19961);
        // out-of-range stub index → null
        assert!(rift_get_stub(h, 19961, cstr(r#"{"index":99}"#).as_ptr()).is_null());
        // malformed ref JSON → null
        assert!(rift_get_stub(h, 19961, cstr("not json").as_ptr()).is_null());
        // malformed stub JSON on add → -1
        assert_eq!(rift_add_stub(h, 19961, cstr("not json").as_ptr(), -1), -1);
        rift_stop(h);
    }
}

#[test]
fn ffi_admin_longtail_projections_and_edges() {
    unsafe {
        let h = rift_start();
        // Imposter with a pure-proxy stub + a regular stub, to exercise removeProxies stripping.
        let cfg = cstr(
            r#"{ "port": 19962, "protocol": "http", "recordRequests": true, "stubs": [
                 { "id": "p", "predicates": [{ "equals": { "path": "/px" } }],
                   "responses": [{ "proxy": { "to": "http://127.0.0.1:1", "mode": "proxyOnce" } }] },
                 { "id": "r", "responses": [{ "is": { "statusCode": 200, "body": "r" } }] } ] }"#,
        );
        let port = rift_create_imposter(h, cfg.as_ptr());
        assert_eq!(port, 19962);

        let stub_count = |json: &str, replayable_wrapper: bool| -> usize {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            if replayable_wrapper {
                v["imposters"][0]["stubs"].as_array().unwrap().len()
            } else {
                v["stubs"].as_array().unwrap().len()
            }
        };

        // list, replayable: both stubs present; +removeProxies: the pure-proxy stub is dropped.
        let l_all = take_json(rift_list_imposters(
            h,
            cstr(r#"{"replayable":true}"#).as_ptr(),
        ));
        assert_eq!(stub_count(&l_all, true), 2);
        let l_np = take_json(rift_list_imposters(
            h,
            cstr(r#"{"replayable":true,"removeProxies":true}"#).as_ptr(),
        ));
        assert_eq!(
            stub_count(&l_np, true),
            1,
            "removeProxies strips the proxy stub (list)"
        );

        // get, replayable + removeProxies — replayable returns the bare config (top-level stubs).
        let g_np = take_json(rift_get_imposter(
            h,
            port,
            cstr(r#"{"replayable":true,"removeProxies":true}"#).as_ptr(),
        ));
        assert_eq!(stub_count(&g_np, false), 1);
        // get, DETAIL (non-replayable) + removeProxies — must ALSO strip, matching the admin route.
        let g_detail_np = take_json(rift_get_imposter(
            h,
            port,
            cstr(r#"{"removeProxies":true}"#).as_ptr(),
        ));
        assert_eq!(
            stub_count(&g_detail_np, false),
            1,
            "removeProxies strips on the detail view too"
        );
        let g_detail = take_json(rift_get_imposter(h, port, std::ptr::null()));
        assert_eq!(
            stub_count(&g_detail, false),
            2,
            "no removeProxies keeps both"
        );

        // A misspelled option key is a hard error (deny_unknown_fields), not a silent default.
        assert!(rift_list_imposters(h, cstr(r#"{"repayable":true}"#).as_ptr()).is_null());
        assert!(rift_get_imposter(h, port, cstr(r#"{"bogus":true}"#).as_ptr()).is_null());

        // Stub addressing: exercise the OTHER StubRef branch of update/delete than the main test.
        // update by INDEX (main test does update by id):
        let upd =
            cstr(r#"{ "id": "r", "responses": [{ "is": { "statusCode": 204, "body": "" } }] }"#);
        assert_eq!(
            rift_update_stub(h, port, cstr(r#"{"index":1}"#).as_ptr(), upd.as_ptr()),
            0
        );
        // add at an EXPLICIT index 0 (main test appends with -1), then confirm order:
        let ins = cstr(r#"{ "id": "first", "responses": [{ "is": { "statusCode": 200 } }] }"#);
        assert_eq!(rift_add_stub(h, port, ins.as_ptr(), 0), 0);
        let after = take_json(rift_get_stub(h, port, cstr(r#"{"index":0}"#).as_ptr()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&after).unwrap()["id"],
            "first",
            "explicit index 0 inserts at the front"
        );
        // delete by ID (main test deletes by index):
        assert_eq!(
            rift_delete_stub(h, port, cstr(r#"{"id":"first"}"#).as_ptr()),
            0
        );
        // unknown id → null
        assert!(rift_get_stub(h, port, cstr(r#"{"id":"nope"}"#).as_ptr()).is_null());

        rift_stop(h);
    }
}

#[test]
fn ffi_admin_longtail_scenario_and_flow_edges() {
    unsafe {
        let h = rift_start();
        let cfg = cstr(
            r#"{ "port": 19963, "protocol": "http", "stubs": [
                 { "scenarioName": "order", "requiredScenarioState": "Started",
                   "responses": [{ "is": { "statusCode": 200 } }] } ] }"#,
        );
        let port = rift_create_imposter(h, cfg.as_ptr());
        assert_eq!(port, 19963);

        // missing "state" field → -1 with a specific message.
        assert_eq!(
            rift_set_scenario_state(h, port, cstr("order").as_ptr(), cstr("{}").as_ptr()),
            -1
        );

        // Explicit flowId isolates state from the default flow.
        assert_eq!(
            rift_set_scenario_state(
                h,
                port,
                cstr("order").as_ptr(),
                cstr(r#"{"state":"paid","flowId":"tenant-a"}"#).as_ptr(),
            ),
            0
        );
        let scen_a = take_json(rift_scenarios(h, port, cstr("tenant-a").as_ptr()));
        let va: serde_json::Value = serde_json::from_str(&scen_a).unwrap();
        assert_eq!(va["flowId"], "tenant-a");
        assert_eq!(va["scenarios"][0]["state"], "paid");
        // The default flow is untouched (still initial "Started").
        let scen_def = take_json(rift_scenarios(h, port, std::ptr::null()));
        let vd: serde_json::Value = serde_json::from_str(&scen_def).unwrap();
        assert_eq!(
            vd["scenarios"][0]["state"], "Started",
            "default flow isolated from tenant-a"
        );

        // Error-path coverage for the symbols the happy-path test only exercises positively.
        assert!(
            rift_update_stub(std::ptr::null_mut(), 1, std::ptr::null(), std::ptr::null()) == -1
        );
        assert!(rift_delete_stub(std::ptr::null_mut(), 1, std::ptr::null()) == -1);
        assert_eq!(rift_clear_proxy_recordings(std::ptr::null_mut(), 1), -1);
        assert!(rift_scenarios(std::ptr::null_mut(), 1, std::ptr::null()).is_null());
        assert_eq!(
            rift_set_scenario_state(std::ptr::null_mut(), 1, std::ptr::null(), std::ptr::null()),
            -1
        );
        assert_eq!(
            rift_reset_scenarios(std::ptr::null_mut(), 1, std::ptr::null()),
            -1
        );
        // unknown port
        assert!(rift_scenarios(h, 12345, std::ptr::null()).is_null());
        assert_eq!(rift_clear_proxy_recordings(h, 12345), -1);
        assert_eq!(rift_reset_scenarios(h, 12345, std::ptr::null()), -1);

        rift_stop(h);
    }
}
