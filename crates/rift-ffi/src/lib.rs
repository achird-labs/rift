//! Rift C-ABI (issue #204, extended in #343): a tiny opaque-handle + JSON in / JSON out FFI over
//! the rift-mock-core engine, so a host (e.g. the JVM via Panama FFM) can embed Rift in-process.
//!
//! Boundary discipline:
//! - **Opaque handle + JSON only.** No Rust enums/generics cross the line; the JSON codec is the
//!   same `rift-types`/`rift-mock-core` wire model the admin API uses, so it is version-tolerant.
//! - **Memory created and freed on the same side.** `rift_start`/`rift_stop` own the
//!   [`RiftHandle`]; string returns are `*mut c_char` the caller hands back to [`rift_free`]
//!   (the one exception is [`rift_build_info`], a static string that is never freed).
//! - **No futures cross the boundary.** The handle owns a multi-thread Tokio runtime; mutating
//!   downcalls are blocking `block_on` calls (read-only ones like [`rift_recorded`] are plain
//!   synchronous reads), so the host wraps them in its own blocking effect.
//! - **One handle is safe to share across host threads:** the engine is `Sync` and every
//!   downcall takes `&self`, so concurrent calls on the same handle are permitted.
//!
//! ## C-ABI v2 (issue #343)
//! - `rift_serve_admin` spawns the *real* [`AdminApiServer`] on the handle's runtime over the
//!   handle's manager, so an embedded host gets the byte-identical admin surface (spaces,
//!   scenarios, flow-state, the `/__rift/` gateway, …) and inherits future admin features.
//! - `rift_apply_config`, `rift_delete_imposter` extend the control surface; `rift_build_info`
//!   exposes build identity; `rift_last_error` surfaces the reason a `rift_*` call failed.
//! - The seven v1 symbols keep their exact signatures and semantics; v2 detection is the presence
//!   of `rift_build_info`. Every *operation* entry clears the thread-local last-error and every
//!   failure sets it (v1 functions too — additive, their return values are unchanged); the pure
//!   accessors and `rift_free` leave it untouched so read-then-free is order-independent.
//!
//! Every `extern "C"` function is wrapped in a panic guard ([`ffi_guard!`], issue #484): a Rust
//! panic never unwinds across the boundary — it is caught, its message recorded in
//! [`rift_last_error`], and the function's sentinel returned — so an engine bug degrades to a clean
//! error instead of crashing or corrupting the host runtime. The handle's locks are non-poisoning
//! (`parking_lot`) so a caught panic can't wedge every later call on the handle. A failed downcall
//! (error or caught panic) returns its sentinel, records the reason in [`rift_last_error`], and
//! emits a `tracing` event.

use parking_lot::Mutex;
use rift_http_proxy::admin_api::{
    AdminApiServer, RunningAdminApi, filter_proxy_responses, filter_proxy_stubs,
};
use rift_http_proxy::config_loader::{self, ConfigSource};
use rift_http_proxy::intercept_control::{
    InterceptControl, InterceptStartError, InterceptStartOptions, InterceptStatus,
};
use rift_http_proxy::intercept_rules::InterceptRule;
use rift_http_proxy::server::{RunningMetrics, bind_metrics_server};
use rift_mock_core::imposter::{
    ApplyReport, Imposter, ImposterConfig, ImposterManager, Stub, VerifyOptions,
};
use rift_mock_core::proxy::truststore::{TrustStorePassword, ca_pem, export_jks, export_pkcs12};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;
use tracing::warn;

/// Run an FFI body under a panic guard (issue #484): a Rust panic must never unwind across the
/// `extern "C"` boundary (that is undefined behaviour). A caught panic records its message in
/// [`rift_last_error`], emits a `tracing` event, and returns the function's `$sentinel`
/// (null / `0` / `-1` / `()`), so the host sees a clean failure instead of a corrupted runtime.
macro_rules! ffi_guard {
    ($name:literal, $sentinel:expr, $body:expr) => {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(value) => value,
            Err(payload) => {
                let cause = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic");
                set_last_error(format!("{}: panicked: {cause}", $name));
                warn!(panic = %cause, function = $name, "rift-ffi: caught panic at the FFI boundary");
                $sentinel
            }
        }
    };
}

thread_local! {
    /// The reason the most recent `rift_*` operation on THIS thread failed, or `None`. Operation
    /// entries clear this and their failures set it; the accessors and `rift_free` leave it alone
    /// (see [`clear_last_error`]).
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Clear this thread's last-error. Called on entry to every *operation* extern fn. The pure
/// accessors ([`rift_last_error`], [`rift_build_info`]) and the deallocator ([`rift_free`]) do
/// NOT clear it, so "check the sentinel, read `rift_last_error`, then `rift_free` the buffer" is
/// order-independent.
fn clear_last_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

/// Record this thread's last-error message (interior NULs are replaced with a fixed note).
fn set_last_error(msg: impl AsRef<str>) {
    let c = CString::new(msg.as_ref()).unwrap_or_else(|_| {
        CString::new("error message contained an interior NUL").expect("no NUL")
    });
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

/// Opaque handle: a Tokio runtime (on its own threads), the engine it drives, and — once
/// [`rift_serve_admin`] is called — the in-process admin/metrics plane serving over that engine.
pub struct RiftHandle {
    runtime: Runtime,
    manager: Arc<ImposterManager>,
    admin: Mutex<Option<AdminPlane>>,
    /// The handle's single intercept plane, shared with its embedded admin plane (issue #493): a
    /// listener started via `rift_start_intercept` is visible to `GET /intercept` on the admin
    /// plane, and vice versa — one slot, driven by both the C-ABI and the HTTP surface.
    intercept: InterceptControl,
}

/// The admin API (and optional metrics server) serving in-process for a handle (issue #343).
struct AdminPlane {
    admin: RunningAdminApi,
    metrics: Option<RunningMetrics>,
}

/// Borrow the handle from a raw pointer, or `None` if null.
///
/// # Safety
/// `h` must be null or a pointer returned by [`rift_start`] and not yet passed to [`rift_stop`].
unsafe fn handle<'a>(h: *mut RiftHandle) -> Option<&'a RiftHandle> {
    unsafe { h.as_ref() }
}

/// Read a borrowed UTF-8 string from a C string pointer, or `None` if null/invalid.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated C string that outlives the borrow.
unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    unsafe {
        if p.is_null() {
            return None;
        }
        CStr::from_ptr(p).to_str().ok()
    }
}

/// Move a `String` across the boundary as an owned `*mut c_char` the caller frees with
/// [`rift_free`]. Returns null if the string contains an interior NUL, recording the reason in
/// `rift_last_error` so a null return always carries a diagnostic — the contract every
/// string-returning entry point advertises (null means error, reason in `rift_last_error`).
fn into_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(e) => {
            set_last_error(format!(
                "internal error: response contained an interior NUL byte ({e})"
            ));
            std::ptr::null_mut()
        }
    }
}

/// Parse an optional JSON options object: `p` null yields `T::default()`; a non-null pointer is
/// parsed and any failure (invalid UTF-8 or invalid JSON) sets `last_error` (prefixed with
/// `fn_name`) and returns `None`. Shared by the `rift_list_imposters`/`rift_get_imposter`
/// `{"replayable","removeProxies"}` projection (issue #491).
///
/// # Safety
/// `p` must be null or a valid NUL-terminated C string.
unsafe fn parse_opts<T: serde::de::DeserializeOwned + Default>(
    p: *const c_char,
    fn_name: &str,
) -> Option<T> {
    unsafe {
        if p.is_null() {
            return Some(T::default());
        }
        let Some(s) = c_str(p) else {
            set_last_error(format!("{fn_name}: options is not valid UTF-8"));
            return None;
        };
        match serde_json::from_str(s) {
            Ok(v) => Some(v),
            Err(e) => {
                set_last_error(format!("{fn_name}: invalid options JSON: {e}"));
                None
            }
        }
    }
}

/// Resolve a nullable `flow_id` C-string arg (issue #491): null → the imposter's default flow; a
/// valid string → itself; a non-null but invalid-UTF-8 pointer → `None` after recording an error,
/// so the caller surfaces a sentinel rather than silently acting on the WRONG (default) flow.
///
/// # Safety
/// `flow_id` must be null or a valid NUL-terminated C string.
unsafe fn resolve_flow_arg(
    flow_id: *const c_char,
    imposter: &Imposter,
    fn_name: &str,
) -> Option<String> {
    unsafe {
        if flow_id.is_null() {
            return Some(imposter.resolve_flow_id(&std::collections::HashMap::new()));
        }
        match c_str(flow_id) {
            Some(s) => Some(s.to_string()),
            None => {
                set_last_error(format!("{fn_name}: flow_id is not valid UTF-8"));
                None
            }
        }
    }
}

/// `rift_serve_admin` options (all fields optional). Typed so a wrong-JSON-type field is a serde
/// error surfaced via `last_error` — not silently coerced to a default (a non-string `apiKey`
/// silently disabling auth, or an out-of-range `port` truncating, would be a real footgun).
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ServeOptions {
    host: Option<String>,
    port: Option<u16>,
    api_key: Option<String>,
    metrics_port: Option<u16>,
    config_file: Option<String>,
    config: Option<Value>,
    /// Gate script/inject imposters submitted THROUGH the admin plane (issue #492). Default false,
    /// preserving the previous behavior. Direct FFI calls (`rift_create_imposter`, …) are ungated
    /// by design — the host process is already trusted; this only governs the in-process admin API.
    allow_injection: Option<bool>,
}

/// Parse `{"imposters":[...]}` or a bare `[...]` into imposter configs (the reload input shape).
fn parse_imposter_configs(v: &Value) -> Result<Vec<ImposterConfig>, String> {
    let array = v.get("imposters").unwrap_or(v);
    serde_json::from_value::<Vec<ImposterConfig>>(array.clone()).map_err(|e| e.to_string())
}

/// Start the engine. Returns an opaque handle, or null if the runtime could not be created.
///
/// Installs the process-wide rustls `ring` crypto provider (idempotently) so HTTPS imposters
/// created through this handle work without the host installing a provider itself (issue #343).
#[unsafe(no_mangle)]
pub extern "C" fn rift_start() -> *mut RiftHandle {
    ffi_guard!("rift_start", std::ptr::null_mut(), {
        clear_last_error();
        rift_http_proxy::install_default_crypto_provider();
        match Runtime::new() {
            Ok(runtime) => Box::into_raw(Box::new(RiftHandle {
                runtime,
                manager: Arc::new(ImposterManager::new()),
                admin: Mutex::new(None),
                intercept: InterceptControl::default(),
            })),
            Err(e) => {
                set_last_error(format!("rift_start: runtime creation failed: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Create an imposter from a JSON config. Returns its port, or `0` on any error
/// (null handle/json, malformed config, or bind failure — `0` is never a live imposter port).
///
/// # Safety
/// `h` must be a live handle and `json` a valid C string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_create_imposter(h: *mut RiftHandle, json: *const c_char) -> u16 {
    ffi_guard!("rift_create_imposter", 0, unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(json)) else {
            set_last_error("rift_create_imposter: null handle or config pointer");
            warn!("rift_create_imposter: null handle or config pointer");
            return 0;
        };
        let config = match serde_json::from_str::<ImposterConfig>(s) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("rift_create_imposter: invalid config JSON: {e}"));
                warn!(error = %e, "rift_create_imposter: invalid config JSON");
                return 0;
            }
        };
        match handle
            .runtime
            .block_on(handle.manager.create_imposter(config))
        {
            Ok(port) => port,
            Err(e) => {
                set_last_error(format!("rift_create_imposter: {e}"));
                warn!(error = %e, "rift_create_imposter failed");
                0
            }
        }
    })
}

/// Replace all stubs on `port` from a JSON array. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle and `json` a valid C string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_replace_stubs(
    h: *mut RiftHandle,
    port: u16,
    json: *const c_char,
) -> i32 {
    ffi_guard!("rift_replace_stubs", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(json)) else {
            set_last_error("rift_replace_stubs: null handle or stubs pointer");
            warn!("rift_replace_stubs: null handle or stubs pointer");
            return -1;
        };
        let stubs = match serde_json::from_str::<Vec<Stub>>(s) {
            Ok(v) => v,
            Err(e) => {
                set_last_error(format!("rift_replace_stubs: invalid stubs JSON: {e}"));
                warn!(error = %e, "rift_replace_stubs: invalid stubs JSON");
                return -1;
            }
        };
        match handle
            .runtime
            .block_on(handle.manager.replace_stubs(port, stubs))
        {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_replace_stubs: {e}"));
                warn!(error = %e, port, "rift_replace_stubs failed");
                -1
            }
        }
    })
}

/// Remove all imposters. Returns `0` on success, `-1` if the handle is null.
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_delete_all(h: *mut RiftHandle) -> i32 {
    ffi_guard!("rift_delete_all", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_delete_all: null handle");
            return -1;
        };
        handle.runtime.block_on(handle.manager.delete_all());
        0
    })
}

/// Delete one imposter, freeing its port. Returns `0` on success, `-1` on any error
/// (null handle or no imposter on `port`). Issue #343.
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_delete_imposter(h: *mut RiftHandle, port: u16) -> i32 {
    ffi_guard!("rift_delete_imposter", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_delete_imposter: null handle");
            return -1;
        };
        match handle
            .runtime
            .block_on(handle.manager.delete_imposter(port))
        {
            Ok(_) => 0,
            Err(e) => {
                set_last_error(format!("rift_delete_imposter: {e}"));
                warn!(error = %e, port, "rift_delete_imposter failed");
                -1
            }
        }
    })
}

/// Return the recorded requests for `port` as a JSON array string the caller must free with
/// [`rift_free`]. Returns null on any error (null/unknown handle or port, or encode failure).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_recorded(h: *mut RiftHandle, port: u16) -> *mut c_char {
    ffi_guard!("rift_recorded", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_recorded: null handle");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_recorded: {e}"));
                warn!(error = %e, port, "rift_recorded: no such imposter");
                return std::ptr::null_mut();
            }
        };
        match serde_json::to_string(&imposter.get_recorded_requests()) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_recorded: encode failed: {e}"));
                warn!(error = %e, port, "rift_recorded: failed to encode recorded requests");
                std::ptr::null_mut()
            }
        }
    })
}

/// Return the stub-overlap analysis warnings for `port` as a JSON array string the caller must free
/// with [`rift_free`] (issue #423). This gives embedded / direct C-ABI consumers the config-lint
/// warnings (duplicate/shadowed/catch-all stubs) that previously only the HTTP admin layer
/// surfaced. The warnings are computed once on stub mutation and cached, so this is a cheap read.
/// Returns null on any error (null/unknown handle or port, or encode failure).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_stub_warnings(h: *mut RiftHandle, port: u16) -> *mut c_char {
    ffi_guard!("rift_stub_warnings", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_stub_warnings: null handle");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_stub_warnings: {e}"));
                warn!(error = %e, port, "rift_stub_warnings: no such imposter");
                return std::ptr::null_mut();
            }
        };
        match serde_json::to_string(&*imposter.stub_warnings()) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_stub_warnings: encode failed: {e}"));
                warn!(error = %e, port, "rift_stub_warnings: failed to encode warnings");
                std::ptr::null_mut()
            }
        }
    })
}

/// Verify recorded requests against a predicate set server-side (issue #494), returning the JSON
/// `{"matched","total","requests"?,"closest"?}` envelope the caller frees with [`rift_free`]. The
/// `body_json` is the same `POST /imposters/{port}/verify` body:
/// `{"predicates":[…],"flowId"?,"includeRequests"?,"includeClosest"?}`. This lets an embedded SDK
/// count matches through the engine's one true predicate evaluator (including `xpath`/`inject`,
/// impractical client-side) instead of shipping the whole journal over the wire. Unlike the admin
/// HTTP endpoint, an `inject` predicate is NOT gated here: the direct C-ABI caller is the trusted
/// in-process embedder, matching how `rift_replace_stubs` accepts inject stubs.
///
/// Returns null on any error (null/unknown handle or port, invalid JSON, a failing `inject`
/// predicate, or encode failure), with the reason in `rift_last_error`.
///
/// # Safety
/// `h` must be a live handle (or null); `body_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_verify(
    h: *mut RiftHandle,
    port: u16,
    body_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_verify", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(body_json)) else {
            set_last_error("rift_verify: null handle or body pointer");
            warn!("rift_verify: null handle or body pointer");
            return std::ptr::null_mut();
        };
        let opts = match serde_json::from_str::<VerifyOptions>(s) {
            Ok(o) => o,
            Err(e) => {
                set_last_error(format!("rift_verify: invalid verify JSON: {e}"));
                warn!(error = %e, "rift_verify: invalid verify JSON");
                return std::ptr::null_mut();
            }
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_verify: {e}"));
                warn!(error = %e, port, "rift_verify: no such imposter");
                return std::ptr::null_mut();
            }
        };
        let outcome = match imposter.verify(&opts) {
            Ok(o) => o,
            Err(e) => {
                set_last_error(format!("rift_verify: {e}"));
                warn!(error = %e, port, "rift_verify: predicate evaluation failed");
                return std::ptr::null_mut();
            }
        };
        match serde_json::to_string(&outcome) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_verify: encode failed: {e}"));
                warn!(error = %e, port, "rift_verify: failed to encode outcome");
                std::ptr::null_mut()
            }
        }
    })
}

// ── Admin long tail over direct C-ABI: imposter list/get, stub surgery, clear/enable, scenarios
// (issue #491) ────────────────────────────────────────────────────────────────────────────────
// Each function calls the same `ImposterManager`/`Imposter` method the corresponding admin-HTTP
// handler calls (`crates/rift-http-proxy/src/admin_api/handlers/imposters.rs`/`scenarios.rs`) and
// builds the same JSON shape, so an embedder gets full admin parity with zero loopback HTTP.

/// A stub reference: `{"index":N}` (position) or `{"id":"..."}` (stable id) — the two ways
/// `rift_get_stub`/`rift_update_stub`/`rift_delete_stub` address a stub.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StubRef {
    Index { index: usize },
    Id { id: String },
}

/// `rift_list_imposters`/`rift_get_imposter` options (all optional, default `false`): `replayable`
/// returns the full `ImposterConfig` projection instead of the summary/detail shape;
/// `removeProxies` (with `replayable`) strips proxy responses via the SAME
/// [`filter_proxy_responses`] the admin `?replayable=true&removeProxies=true` route uses.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ImposterProjection {
    replayable: bool,
    remove_proxies: bool,
}

/// List imposters. `options_json` (null = defaults): `{"replayable":bool,"removeProxies":bool}`.
/// Replayable returns `{"imposters":[<ImposterConfig>,...]}` (the same projection the admin
/// `?replayable=true` route serves); otherwise a Mountebank-style summary
/// `{"imposters":[{"protocol","port","name"?,"numberOfRequests","enabled"},...]}`, skipping any
/// imposter with no assigned port (mirroring `handle_list`'s summary branch). Returns (caller
/// frees with [`rift_free`]) null on any error (null handle or malformed options JSON).
///
/// # Safety
/// `h` must be a live handle (or null); `options_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_list_imposters(
    h: *mut RiftHandle,
    options_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_list_imposters", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_list_imposters: null handle");
            return std::ptr::null_mut();
        };
        let Some(opts) = parse_opts::<ImposterProjection>(options_json, "rift_list_imposters")
        else {
            return std::ptr::null_mut();
        };
        let imposters = handle.manager.list_imposters();
        let body = if opts.replayable {
            let configs: Vec<ImposterConfig> = imposters
                .iter()
                .map(|i| {
                    if opts.remove_proxies {
                        filter_proxy_responses(&i.config)
                    } else {
                        i.config.clone()
                    }
                })
                .collect();
            json!({ "imposters": configs })
        } else {
            let summaries: Vec<Value> = imposters
                .iter()
                .filter_map(|i| {
                    i.config.port.map(|port| {
                        let mut entry = json!({
                            "protocol": i.config.protocol,
                            "port": port,
                            "numberOfRequests": i.get_request_count(),
                            "enabled": i.is_enabled(),
                        });
                        if let Some(name) = &i.config.name {
                            entry["name"] = json!(name);
                        }
                        entry
                    })
                })
                .collect();
            json!({ "imposters": summaries })
        };
        match serde_json::to_string(&body) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_list_imposters: encode failed: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Get one imposter. `options_json` — same shape as [`rift_list_imposters`]. Replayable returns
/// the single `ImposterConfig` (same `removeProxies` projection); otherwise a detail object
/// `{"protocol","port","name"?,"numberOfRequests","enabled","recordRequests","stubs","requests"}`.
/// Returns (caller frees) null on any error (null handle, unknown port, or malformed options).
///
/// # Safety
/// `h` must be a live handle (or null); `options_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_get_imposter(
    h: *mut RiftHandle,
    port: u16,
    options_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_get_imposter", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_get_imposter: null handle");
            return std::ptr::null_mut();
        };
        let Some(opts) = parse_opts::<ImposterProjection>(options_json, "rift_get_imposter") else {
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_get_imposter: {e}"));
                warn!(error = %e, port, "rift_get_imposter: no such imposter");
                return std::ptr::null_mut();
            }
        };
        let body = if opts.replayable {
            let config = if opts.remove_proxies {
                filter_proxy_responses(&imposter.config)
            } else {
                imposter.config.clone()
            };
            match serde_json::to_value(&config) {
                Ok(v) => v,
                Err(e) => {
                    set_last_error(format!("rift_get_imposter: encode failed: {e}"));
                    return std::ptr::null_mut();
                }
            }
        } else {
            // Honor `removeProxies` on the detail view too, filtering the LIVE stubs — the admin
            // `GET /imposters/{port}?removeProxies=true` applies it regardless of `replayable`.
            let stubs = if opts.remove_proxies {
                filter_proxy_stubs(imposter.get_stubs())
            } else {
                imposter.get_stubs()
            };
            let mut detail = json!({
                "protocol": imposter.config.protocol,
                "port": port,
                "numberOfRequests": imposter.get_request_count(),
                "enabled": imposter.is_enabled(),
                "recordRequests": imposter.config.record_requests,
                "stubs": stubs,
                "requests": imposter.get_recorded_requests(),
            });
            if let Some(name) = &imposter.config.name {
                detail["name"] = json!(name);
            }
            detail
        };
        match serde_json::to_string(&body) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_get_imposter: encode failed: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// Add a stub to `port`. `index < 0` appends; otherwise it is inserted at that position (mirrors
/// the admin route's `index` query param). No injection gating — direct FFI is the trusted
/// embedder, like [`rift_replace_stubs`] — and no stub id is auto-generated. Returns `0` on
/// success, `-1` on any error (null handle/pointer, invalid stub JSON, or the manager's own error,
/// e.g. a duplicate id).
///
/// # Safety
/// `h` must be a live handle (or null); `stub_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_add_stub(
    h: *mut RiftHandle,
    port: u16,
    stub_json: *const c_char,
    index: i32,
) -> i32 {
    ffi_guard!("rift_add_stub", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(stub_json)) else {
            set_last_error("rift_add_stub: null handle or stub pointer");
            return -1;
        };
        let stub = match serde_json::from_str::<Stub>(s) {
            Ok(v) => v,
            Err(e) => {
                set_last_error(format!("rift_add_stub: invalid stub JSON: {e}"));
                return -1;
            }
        };
        let idx = if index < 0 {
            None
        } else {
            Some(index as usize)
        };
        match handle
            .runtime
            .block_on(handle.manager.add_stub(port, stub, idx))
        {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_add_stub: {e}"));
                warn!(error = %e, port, "rift_add_stub failed");
                -1
            }
        }
    })
}

/// Get a single stub by ref: `{"index":N}` or `{"id":"..."}`. Returns (caller frees) the bare
/// `Stub` JSON, or null on any error (null handle/pointer, malformed ref JSON, out-of-range index,
/// or unknown id).
///
/// # Safety
/// `h` must be a live handle (or null); `ref_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_get_stub(
    h: *mut RiftHandle,
    port: u16,
    ref_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_get_stub", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(ref_json)) else {
            set_last_error("rift_get_stub: null handle or ref pointer");
            return std::ptr::null_mut();
        };
        let stub_ref = match serde_json::from_str::<StubRef>(s) {
            Ok(r) => r,
            Err(e) => {
                set_last_error(format!("rift_get_stub: invalid ref JSON: {e}"));
                return std::ptr::null_mut();
            }
        };
        let result = match stub_ref {
            StubRef::Index { index } => handle.manager.get_stub(port, index),
            StubRef::Id { id } => handle.manager.get_stub_by_id(port, &id),
        };
        match result {
            Ok(stub) => match serde_json::to_string(&stub) {
                Ok(json) => into_c_string(json),
                Err(e) => {
                    set_last_error(format!("rift_get_stub: encode failed: {e}"));
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                set_last_error(format!("rift_get_stub: {e}"));
                warn!(error = %e, port, "rift_get_stub failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Update (replace) a stub addressed by ref (`{"index":N}` or `{"id":"..."}`) with `stub_json`.
/// Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); `ref_json`/`stub_json` must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_update_stub(
    h: *mut RiftHandle,
    port: u16,
    ref_json: *const c_char,
    stub_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_update_stub", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(rs), Some(ss)) = (handle(h), c_str(ref_json), c_str(stub_json))
        else {
            set_last_error("rift_update_stub: null handle or string pointer");
            return -1;
        };
        let stub_ref = match serde_json::from_str::<StubRef>(rs) {
            Ok(r) => r,
            Err(e) => {
                set_last_error(format!("rift_update_stub: invalid ref JSON: {e}"));
                return -1;
            }
        };
        let stub = match serde_json::from_str::<Stub>(ss) {
            Ok(s) => s,
            Err(e) => {
                set_last_error(format!("rift_update_stub: invalid stub JSON: {e}"));
                return -1;
            }
        };
        let result = match stub_ref {
            StubRef::Index { index } => handle
                .runtime
                .block_on(handle.manager.replace_stub(port, index, stub)),
            StubRef::Id { id } => handle
                .runtime
                .block_on(handle.manager.replace_stub_by_id(port, &id, stub)),
        };
        match result {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_update_stub: {e}"));
                warn!(error = %e, port, "rift_update_stub failed");
                -1
            }
        }
    })
}

/// Delete a stub addressed by ref (`{"index":N}` or `{"id":"..."}`). Returns `0` on success, `-1`
/// on any error.
///
/// # Safety
/// `h` must be a live handle (or null); `ref_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_delete_stub(
    h: *mut RiftHandle,
    port: u16,
    ref_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_delete_stub", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(ref_json)) else {
            set_last_error("rift_delete_stub: null handle or ref pointer");
            return -1;
        };
        let stub_ref = match serde_json::from_str::<StubRef>(s) {
            Ok(r) => r,
            Err(e) => {
                set_last_error(format!("rift_delete_stub: invalid ref JSON: {e}"));
                return -1;
            }
        };
        let result = match stub_ref {
            StubRef::Index { index } => handle
                .runtime
                .block_on(handle.manager.delete_stub(port, index)),
            StubRef::Id { id } => handle
                .runtime
                .block_on(handle.manager.delete_stub_by_id(port, &id)),
        };
        match result {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_delete_stub: {e}"));
                warn!(error = %e, port, "rift_delete_stub failed");
                -1
            }
        }
    })
}

/// Clear all recorded requests for `port`. Returns `0` on success, `-1` on any error (null handle
/// or unknown port).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_clear_recorded(h: *mut RiftHandle, port: u16) -> i32 {
    ffi_guard!("rift_clear_recorded", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_clear_recorded: null handle");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_clear_recorded: {e}"));
                return -1;
            }
        };
        match imposter.clear_recorded_requests() {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_clear_recorded: {e}"));
                warn!(error = %e, port, "rift_clear_recorded failed");
                -1
            }
        }
    })
}

/// Clear saved proxy responses for `port`. Returns `0` on success, `-1` on any error (null handle
/// or unknown port).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_clear_proxy_recordings(h: *mut RiftHandle, port: u16) -> i32 {
    ffi_guard!("rift_clear_proxy_recordings", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_clear_proxy_recordings: null handle");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_clear_proxy_recordings: {e}"));
                return -1;
            }
        };
        imposter.clear_proxy_responses();
        0
    })
}

/// Enable (`enabled != 0`) or disable an imposter. Returns `0` on success, `-1` on any error (null
/// handle or unknown port).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_set_imposter_enabled(
    h: *mut RiftHandle,
    port: u16,
    enabled: i32,
) -> i32 {
    ffi_guard!("rift_set_imposter_enabled", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_set_imposter_enabled: null handle");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_set_imposter_enabled: {e}"));
                return -1;
            }
        };
        imposter.set_enabled(enabled != 0);
        0
    })
}

/// List scenario states for `flow_id` (null → the imposter's default flow) as JSON
/// `{"flowId","scenarios":[{"name","state"}]}`. Returns (caller frees) null on any error (null
/// handle, unknown port, or a scenario-state backend error).
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_scenarios(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_scenarios", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_scenarios: null handle");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_scenarios: {e}"));
                warn!(error = %e, port, "rift_scenarios: no such imposter");
                return std::ptr::null_mut();
            }
        };
        let Some(flow) = resolve_flow_arg(flow_id, &imposter, "rift_scenarios") else {
            return std::ptr::null_mut();
        };
        let mut scenarios = Vec::new();
        for name in imposter.scenario_names() {
            match imposter.scenario_state(&flow, &name) {
                Ok(state) => scenarios.push(json!({ "name": name, "state": state })),
                Err(e) => {
                    set_last_error(format!("rift_scenarios: {e}"));
                    warn!(error = %e, port, "rift_scenarios failed");
                    return std::ptr::null_mut();
                }
            }
        }
        into_c_string(json!({ "flowId": flow, "scenarios": scenarios }).to_string())
    })
}

/// Set a scenario's state from JSON `{"state":"...","flowId":"..."?}` (`flowId` optional → the
/// imposter's default flow). Returns `0` on success, `-1` on any error (null handle/pointers,
/// missing `state`, unknown port, or a backend error).
///
/// # Safety
/// `h` must be a live handle (or null); `name`/`state_json` must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_set_scenario_state(
    h: *mut RiftHandle,
    port: u16,
    name: *const c_char,
    state_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_set_scenario_state", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(name), Some(s)) = (handle(h), c_str(name), c_str(state_json))
        else {
            set_last_error("rift_set_scenario_state: null handle or string pointer");
            return -1;
        };
        let payload = match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(e) => {
                set_last_error(format!("rift_set_scenario_state: invalid state JSON: {e}"));
                return -1;
            }
        };
        let Some(state) = payload.get("state").and_then(|v| v.as_str()) else {
            set_last_error("rift_set_scenario_state: missing required field: state");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_set_scenario_state: {e}"));
                return -1;
            }
        };
        let flow = payload
            .get("flowId")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| imposter.resolve_flow_id(&std::collections::HashMap::new()));
        match imposter.set_scenario_state(&flow, name, state) {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_set_scenario_state: {e}"));
                warn!(error = %e, port, "rift_set_scenario_state failed");
                -1
            }
        }
    })
}

/// Reset all scenario states for `flow_id` (null → the imposter's default flow) back to their
/// initial state. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_reset_scenarios(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
) -> i32 {
    ffi_guard!("rift_reset_scenarios", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_reset_scenarios: null handle");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_reset_scenarios: {e}"));
                return -1;
            }
        };
        let Some(flow) = resolve_flow_arg(flow_id, &imposter, "rift_reset_scenarios") else {
            return -1;
        };
        for name in imposter.scenario_names() {
            if let Err(e) = imposter.delete_scenario_state(&flow, &name) {
                set_last_error(format!("rift_reset_scenarios: {e}"));
                warn!(error = %e, port, "rift_reset_scenarios failed");
                return -1;
            }
        }
        0
    })
}

// ── Admin long tail over direct C-ABI: scenario state + correlated spaces (issue #411) ──────────
// Each function calls the same `ImposterManager`/`Imposter` methods the admin-HTTP handlers call
// and returns the same JSON, so an embedder can drive scenario state and correlated spaces with
// zero loopback HTTP (no `rift_serve_admin` needed).

/// Get a scenario/flow-state value as a JSON envelope `{"found","flowId","key","value"}` the caller
/// frees with [`rift_free`]. `found` disambiguates an absent key from a failure: a missing key is a
/// non-error outcome (`{"found":false,"value":null}`), while a null pointer is returned **only** on a
/// genuine error (unknown handle/port or encode failure, reason in `rift_last_error`) — matching
/// [`rift_recorded`] and the rc-returning calls, where null/`-1` means error alone.
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id`/`key` must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_flow_state_get(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
    key: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_flow_state_get", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id), Some(key)) = (handle(h), c_str(flow_id), c_str(key))
        else {
            set_last_error("rift_flow_state_get: null handle or string pointer");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_flow_state_get: {e}"));
                return std::ptr::null_mut();
            }
        };
        match imposter.flow_get(flow_id, key) {
            // An absent key is a first-class result (`found:false`), not an error — `value` is a
            // bare JSON value or null (an `Option<Value>` serializes to the value or JSON null).
            Ok(value) => into_c_string(
                json!({ "found": value.is_some(), "flowId": flow_id, "key": key, "value": value })
                    .to_string(),
            ),
            Err(e) => {
                set_last_error(format!("rift_flow_state_get: {e}"));
                warn!(error = %e, port, "rift_flow_state_get failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Set a scenario/flow-state value from a bare JSON value (`value_json`). Returns `0` on success,
/// `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); the string pointers must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_flow_state_put(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
    key: *const c_char,
    value_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_flow_state_put", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id), Some(key), Some(value_json)) =
            (handle(h), c_str(flow_id), c_str(key), c_str(value_json))
        else {
            set_last_error("rift_flow_state_put: null handle or string pointer");
            return -1;
        };
        let value = match serde_json::from_str::<Value>(value_json) {
            Ok(v) => v,
            Err(e) => {
                set_last_error(format!("rift_flow_state_put: invalid value JSON: {e}"));
                return -1;
            }
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_flow_state_put: {e}"));
                return -1;
            }
        };
        match imposter.flow_set(flow_id, key, value) {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_flow_state_put: {e}"));
                warn!(error = %e, port, "rift_flow_state_put failed");
                -1
            }
        }
    })
}

/// Delete a scenario/flow-state key. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); the string pointers must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_flow_state_delete(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
    key: *const c_char,
) -> i32 {
    ffi_guard!("rift_flow_state_delete", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id), Some(key)) = (handle(h), c_str(flow_id), c_str(key))
        else {
            set_last_error("rift_flow_state_delete: null handle or string pointer");
            return -1;
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_flow_state_delete: {e}"));
                return -1;
            }
        };
        match imposter.flow_delete(flow_id, key) {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_flow_state_delete: {e}"));
                warn!(error = %e, port, "rift_flow_state_delete failed");
                -1
            }
        }
    })
}

/// Register a stub scoped to `flow_id` (its `space` is set from `flow_id`, ignoring any `space`
/// in the JSON, mirroring the admin path). Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); the string pointers must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_space_add_stub(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
    stub_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_space_add_stub", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id), Some(stub_json)) =
            (handle(h), c_str(flow_id), c_str(stub_json))
        else {
            set_last_error("rift_space_add_stub: null handle or string pointer");
            return -1;
        };
        let mut stub = match serde_json::from_str::<Stub>(stub_json) {
            Ok(s) => s,
            Err(e) => {
                set_last_error(format!("rift_space_add_stub: invalid stub JSON: {e}"));
                return -1;
            }
        };
        stub.space = Some(flow_id.to_string());
        match handle
            .runtime
            .block_on(handle.manager.add_stub(port, stub, None))
        {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_space_add_stub: {e}"));
                warn!(error = %e, port, "rift_space_add_stub failed");
                -1
            }
        }
    })
}

/// List a space's scoped stubs as JSON `{"space","stubs":[…]}` the caller frees with
/// [`rift_free`], or null on error.
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_space_list_stubs(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_space_list_stubs", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id)) = (handle(h), c_str(flow_id)) else {
            set_last_error("rift_space_list_stubs: null handle or flow_id pointer");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_space_list_stubs: {e}"));
                return std::ptr::null_mut();
            }
        };
        into_c_string(
            json!({ "space": flow_id, "stubs": imposter.space_stubs(flow_id) }).to_string(),
        )
    })
}

/// Tear down a space in one call (its scoped stubs, recorded requests, and scenario state — never
/// a global reset). Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_space_delete(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
) -> i32 {
    ffi_guard!("rift_space_delete", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id)) = (handle(h), c_str(flow_id)) else {
            set_last_error("rift_space_delete: null handle or flow_id pointer");
            return -1;
        };
        match handle
            .runtime
            .block_on(handle.manager.teardown_space(port, flow_id))
        {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!("rift_space_delete: {e}"));
                warn!(error = %e, port, "rift_space_delete failed");
                -1
            }
        }
    })
}

/// The requests recorded for `flow_id` — filtered by the space's resolved flow-id, the same
/// resolution the space-inspection view uses (the header-filtered `received`, issue #201) — as a
/// JSON array the caller frees with [`rift_free`], or null on error.
///
/// # Safety
/// `h` must be a live handle (or null); `flow_id` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_space_recorded(
    h: *mut RiftHandle,
    port: u16,
    flow_id: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_space_recorded", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(flow_id)) = (handle(h), c_str(flow_id)) else {
            set_last_error("rift_space_recorded: null handle or flow_id pointer");
            return std::ptr::null_mut();
        };
        let imposter = match handle.manager.get_imposter(port) {
            Ok(i) => i,
            Err(e) => {
                set_last_error(format!("rift_space_recorded: {e}"));
                return std::ptr::null_mut();
            }
        };
        let recorded: Vec<_> = imposter
            .get_recorded_requests()
            .into_iter()
            .filter(|r| imposter.resolve_flow_id_recorded(&r.headers) == flow_id)
            .collect();
        match serde_json::to_string(&recorded) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_space_recorded: encode failed: {e}"));
                warn!(error = %e, port, "rift_space_recorded: failed to encode");
                std::ptr::null_mut()
            }
        }
    })
}

// ── Intercept/TLS-MITM listener + control plane over FFI (issue #410) ────────────────────────────
// Start an intercept forward-proxy on the handle's runtime and drive its rule store + CA export
// entirely over C-ABI — no loopback HTTP admin plane needed. One listener per handle.

/// A single rule or a batch — `rift_intercept_add_rules` accepts either shape.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum RuleOrRules {
    One(InterceptRule),
    Many(Vec<InterceptRule>),
}

/// Start the intercept/TLS-MITM forward-proxy listener on this handle's runtime. The intercept CA
/// is loaded from `caCertPath`/`caKeyPath` when both are supplied (letting independent instances
/// share a committed trust anchor), and generated fresh otherwise. Returns JSON
/// `{"interceptPort":<u16>,"interceptUrl":"http://<bind-host>:<port>"}` the caller frees with
/// [`rift_free`], or null on error (bad JSON, half-configured CA pair, CA load failure, bind
/// failure, or already started — one listener per handle). `interceptUrl` reflects the bound
/// address (the configured `host`, loopback by default; a `0.0.0.0` bind surfaces verbatim, so
/// dial a concrete interface). `options_json`:
/// `{"host":"127.0.0.1","port":0,"caCertPath":"ca.pem","caKeyPath":"ca.key"}` (port 0 =
/// OS-assigned; CA paths optional, both-or-neither); pass null or `{}` for defaults.
///
/// # Safety
/// `h` must be a live handle (or null); `options_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_start_intercept(
    h: *mut RiftHandle,
    options_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_start_intercept", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_start_intercept: null handle");
            return std::ptr::null_mut();
        };
        let opts: InterceptStartOptions = if options_json.is_null() {
            InterceptStartOptions::default()
        } else {
            match c_str(options_json) {
                Some(s) => match serde_json::from_str(s) {
                    Ok(o) => o,
                    Err(e) => {
                        set_last_error(format!("rift_start_intercept: invalid options JSON: {e}"));
                        return std::ptr::null_mut();
                    }
                },
                None => {
                    set_last_error("rift_start_intercept: options is not valid UTF-8");
                    return std::ptr::null_mut();
                }
            }
        };

        // The shared control serializes concurrent starts and handles CA load + bind; map its
        // typed errors back to the exact `last_error` strings the SDKs match on.
        match handle.runtime.block_on(handle.intercept.start(opts)) {
            Ok(addr) => match serde_json::to_string(&InterceptStatus::from_addr(addr)) {
                Ok(json) => into_c_string(json),
                Err(e) => {
                    set_last_error(format!("rift_start_intercept: encode failed: {e}"));
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                let msg = match &e {
                    InterceptStartError::AlreadyRunning => {
                        "rift_start_intercept: already started (one intercept listener per handle)"
                            .to_string()
                    }
                    InterceptStartError::InvalidAddr(s) => {
                        format!("rift_start_intercept: invalid host/port: {s}")
                    }
                    // `{err:#}` so the chained cause (missing file, bad PEM, mismatched pair)
                    // reaches the caller — `rift_last_error` is their only diagnostic channel.
                    // (`InterceptControl::start` already `warn!`s the CA/bind failure server-side.)
                    InterceptStartError::Ca(err) => {
                        format!("rift_start_intercept: CA setup failed: {err:#}")
                    }
                    InterceptStartError::Bind(err) => {
                        format!("rift_start_intercept: bind failed: {err}")
                    }
                };
                set_last_error(msg);
                std::ptr::null_mut()
            }
        }
    })
}

/// Stop the intercept listener started by [`rift_start_intercept`] (or over the embedded admin
/// plane's `POST /intercept`), releasing its port and dropping its rules + CA — RFC-003 parity with
/// `DELETE /intercept`. Idempotent: stopping when nothing is running is a successful no-op. Returns
/// `0` on success, `-1` only on a null handle or a caught panic. A subsequent
/// [`rift_start_intercept`] without CA paths mints a fresh CA, so re-export the CA afterwards.
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_stop_intercept(h: *mut RiftHandle) -> i32 {
    ffi_guard!("rift_stop_intercept", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_stop_intercept: null handle");
            return -1;
        };
        handle.runtime.block_on(handle.intercept.stop());
        0
    })
}

/// Add one intercept rule (a bare object) or many (a JSON array) — same shape the
/// `/intercept/rules` admin route accepts. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); `rules_json` must be null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_intercept_add_rules(
    h: *mut RiftHandle,
    rules_json: *const c_char,
) -> i32 {
    ffi_guard!("rift_intercept_add_rules", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(rules_json)) = (handle(h), c_str(rules_json)) else {
            set_last_error("rift_intercept_add_rules: null handle or rules pointer");
            return -1;
        };
        let parsed: RuleOrRules = match serde_json::from_str(rules_json) {
            Ok(r) => r,
            Err(e) => {
                set_last_error(format!("rift_intercept_add_rules: invalid rule JSON: {e}"));
                return -1;
            }
        };
        let Some(state) = handle.intercept.state() else {
            set_last_error("rift_intercept_add_rules: intercept not started");
            return -1;
        };
        let result = match parsed {
            RuleOrRules::One(rule) => state.rules.add(rule),
            RuleOrRules::Many(rules) => state.rules.extend(rules),
        };
        if let Err(e) = result {
            set_last_error(format!("rift_intercept_add_rules: {e}"));
            return -1;
        }
        0
    })
}

/// Remove all intercept rules. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_intercept_clear_rules(h: *mut RiftHandle) -> i32 {
    ffi_guard!("rift_intercept_clear_rules", -1, unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_intercept_clear_rules: null handle");
            return -1;
        };
        let Some(state) = handle.intercept.state() else {
            set_last_error("rift_intercept_clear_rules: intercept not started");
            return -1;
        };
        state.rules.clear();
        0
    })
}

/// List the current intercept rules as a JSON array the caller frees with [`rift_free`], or null
/// on error (null handle, intercept not started, or encode failure).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_intercept_list_rules(h: *mut RiftHandle) -> *mut c_char {
    ffi_guard!("rift_intercept_list_rules", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_intercept_list_rules: null handle");
            return std::ptr::null_mut();
        };
        let Some(state) = handle.intercept.state() else {
            set_last_error("rift_intercept_list_rules: intercept not started");
            return std::ptr::null_mut();
        };
        match serde_json::to_string(&state.rules.list()) {
            Ok(json) => into_c_string(json),
            Err(e) => {
                set_last_error(format!("rift_intercept_list_rules: encode failed: {e}"));
                std::ptr::null_mut()
            }
        }
    })
}

/// The intercept CA certificate as PEM, the caller frees with [`rift_free`], or null on error
/// (null handle, or intercept not started).
///
/// # Safety
/// `h` must be a live handle (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_intercept_ca_pem(h: *mut RiftHandle) -> *mut c_char {
    ffi_guard!("rift_intercept_ca_pem", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_intercept_ca_pem: null handle");
            return std::ptr::null_mut();
        };
        let Some(state) = handle.intercept.state() else {
            set_last_error("rift_intercept_ca_pem: intercept not started");
            return std::ptr::null_mut();
        };
        into_c_string(ca_pem(&state.ca))
    })
}

/// Write a truststore for the intercept CA to `out_path` — a truststore is binary, so it is
/// written to a file (the format a JVM `trustStore` consumes directly) rather than returned as a
/// C string. `format` is `"pkcs12"` or `"jks"`; `password` may be null for the default
/// `"changeit"`. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle (or null); the string pointers must be null or valid C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_intercept_export_truststore(
    h: *mut RiftHandle,
    format: *const c_char,
    password: *const c_char,
    out_path: *const c_char,
) -> i32 {
    ffi_guard!("rift_intercept_export_truststore", -1, unsafe {
        clear_last_error();
        let (Some(handle), Some(format), Some(out_path)) =
            (handle(h), c_str(format), c_str(out_path))
        else {
            set_last_error("rift_intercept_export_truststore: null handle or string pointer");
            return -1;
        };
        // Null password -> the default; a present-but-invalid one is an error, not a silent default.
        let password = if password.is_null() {
            "changeit"
        } else {
            match c_str(password) {
                Some(p) => p,
                None => {
                    set_last_error("rift_intercept_export_truststore: password is not valid UTF-8");
                    return -1;
                }
            }
        };
        let Some(state) = handle.intercept.state() else {
            set_last_error("rift_intercept_export_truststore: intercept not started");
            return -1;
        };
        let pw = TrustStorePassword::new(password);
        let bytes = match format {
            "pkcs12" => export_pkcs12(&state.ca, &pw),
            "jks" => export_jks(&state.ca, &pw),
            other => {
                set_last_error(format!(
                    "rift_intercept_export_truststore: unknown format '{other}' (want pkcs12/jks)"
                ));
                return -1;
            }
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                set_last_error(format!(
                    "rift_intercept_export_truststore: export failed: {e}"
                ));
                return -1;
            }
        };
        match std::fs::write(out_path, bytes) {
            Ok(()) => 0,
            Err(e) => {
                set_last_error(format!(
                    "rift_intercept_export_truststore: writing {out_path}: {e}"
                ));
                warn!(error = %e, out_path, "rift_intercept_export_truststore: write failed");
                -1
            }
        }
    })
}

/// Start the real admin API (and, if `metricsPort` is given, the metrics server) in-process on
/// this handle's runtime, serving over this handle's manager (issue #343).
///
/// `options_json` (null or `{}` uses all defaults; every field optional):
/// `{"host":"127.0.0.1","port":0,"apiKey":null,"metricsPort":null,"configFile":null,"config":null}`.
/// `configFile` is loaded and wired as the reload source (like `--configfile`); `config` is an
/// inline `{"imposters":[...]}`. Both are applied via `apply_config` (a reconcile, so a per-port
/// bind failure is reported in the apply report rather than aborting mid-load). They don't
/// compose: passing both leaves only the inline set (its reconcile deletes the rest) — pass one.
///
/// Returns (caller frees) `{"adminPort":49321,"adminUrl":"http://127.0.0.1:49321","metricsPort":null}`,
/// or null on error (bad JSON, bind failure, or already serving — one admin plane per handle).
///
/// # Safety
/// `h` must be a live handle and `options_json` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_serve_admin(
    h: *mut RiftHandle,
    options_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_serve_admin", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let Some(handle) = handle(h) else {
            set_last_error("rift_serve_admin: null handle");
            return std::ptr::null_mut();
        };
        let opts_str = if options_json.is_null() {
            "{}"
        } else {
            match c_str(options_json) {
                Some(s) => s,
                None => {
                    set_last_error("rift_serve_admin: options is not valid UTF-8");
                    return std::ptr::null_mut();
                }
            }
        };
        // Typed parse: a wrong-type field (e.g. a non-string apiKey, an out-of-range port) is a
        // serde error surfaced here, not a silent default.
        let opts: ServeOptions = match serde_json::from_str(opts_str) {
            Ok(o) => o,
            Err(e) => {
                set_last_error(format!("rift_serve_admin: invalid options JSON: {e}"));
                return std::ptr::null_mut();
            }
        };

        // Hold the slot across build+set so a concurrent serve_admin on the same handle can't
        // race two planes into existence (one admin plane per handle).
        let mut slot = handle.admin.lock();
        if slot.is_some() {
            set_last_error("rift_serve_admin: already serving (one admin plane per handle)");
            return std::ptr::null_mut();
        }

        match handle.runtime.block_on(build_admin_plane(handle, &opts)) {
            Ok((plane, response)) => {
                *slot = Some(plane);
                into_c_string(response)
            }
            Err(e) => {
                set_last_error(format!("rift_serve_admin: {e}"));
                warn!(error = %e, "rift_serve_admin failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Log each imposter an `apply_config` left in `report.failed` (e.g. a port that couldn't bind), so
/// a partial apply from serve_admin's configFile/inline config is visible rather than silently
/// dropped — the whole set otherwise applies and serve_admin still succeeds (issue #350).
fn warn_failed(report: &ApplyReport, source: &str) {
    for (port, error) in &report.failed {
        warn!(
            port,
            %error,
            source,
            "rift_serve_admin: config imposter failed to apply (skipped)"
        );
    }
}

/// Bind the admin (and optional metrics) plane per `opts`, returning the plane plus the JSON
/// response body. Errors are `String`s (mapped to `last_error` by the caller).
async fn build_admin_plane(
    handle: &RiftHandle,
    opts: &ServeOptions,
) -> Result<(AdminPlane, String), String> {
    let host = opts.host.as_deref().unwrap_or("127.0.0.1");
    let port = opts.port.unwrap_or(0);
    let api_key = opts.api_key.clone();

    // Parse both addresses up front, before any side effects (imposter creation / binding).
    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .map_err(|e| format!("invalid host/port `{host}:{port}`: {e}"))?;
    let metrics_addr: Option<SocketAddr> = match opts.metrics_port {
        Some(mp) => Some(
            format!("{host}:{mp}")
                .parse()
                .map_err(|e| format!("invalid metrics addr `{host}:{mp}`: {e}"))?,
        ),
        None => None,
    };

    // configFile: apply the loaded set via apply_config, mirroring the inline `config` path and
    // POST /admin/reload. apply_config validates the whole set up front (Err => nothing mutated)
    // and reports per-port failures in its report rather than a half-applied create loop that
    // would return NULL while leaving the already-created imposters behind (issue #350). The
    // source is remembered so POST /admin/reload can re-read it.
    let config_source = match opts.config_file.as_deref() {
        Some(path) => {
            let source = ConfigSource::File {
                path: PathBuf::from(path),
                no_parse: false,
            };
            let configs = config_loader::load_configs(&source)
                .map_err(|e| format!("configFile load: {e}"))?;
            let report = handle
                .manager
                .apply_config(configs)
                .await
                .map_err(|e| format!("configFile apply: {e}"))?;
            warn_failed(&report, "configFile");
            Some(source)
        }
        None => None,
    };

    // Inline config is the desired state applied via apply_config (reconcile), mirroring reload.
    if let Some(cfg) = opts.config.as_ref().filter(|v| !v.is_null()) {
        let configs = parse_imposter_configs(cfg)?;
        let report = handle
            .manager
            .apply_config(configs)
            .await
            .map_err(|e| format!("inline config apply: {e}"))?;
        warn_failed(&report, "inline config");
    }

    // Bind metrics first (optional), then admin — so if the second bind fails, the first
    // listener is explicitly shut down rather than orphaned holding its port.
    let metrics = match metrics_addr {
        Some(maddr) => Some(
            bind_metrics_server(maddr)
                .await
                .map_err(|e| format!("metrics bind: {e}"))?,
        ),
        None => None,
    };

    // Share the handle's intercept slot with the admin plane (issue #493) so `rift_serve_admin`
    // serves the full `/intercept*` surface against the same listener the C-ABI drives: a
    // `rift_start_intercept` is then visible to `GET /intercept`, and `POST /intercept` feeds
    // `rift_intercept_add_rules` — a double-start across surfaces 409s/-1s consistently.
    let mut server = AdminApiServer::new(addr, Arc::clone(&handle.manager), api_key)
        .with_allow_injection(opts.allow_injection.unwrap_or(false))
        .with_intercept(handle.intercept.clone());
    if let Some(source) = config_source {
        server = server.with_config_source(source);
    }
    let admin = match server.bind().await {
        Ok(admin) => admin,
        Err(e) => {
            if let Some(metrics) = metrics {
                metrics.shutdown().await;
            }
            return Err(format!("admin bind: {e}"));
        }
    };
    let admin_addr = admin.local_addr();
    let metrics_port_out = metrics.as_ref().map(|m| m.local_addr().port());

    let response = json!({
        "adminPort": admin_addr.port(),
        "adminUrl": format!("http://{admin_addr}"),
        "metricsPort": metrics_port_out,
    })
    .to_string();

    Ok((AdminPlane { admin, metrics }, response))
}

/// Incrementally reconcile the manager toward the given config (issue #316/#343). Input is
/// `{"imposters":[...]}` or a bare array. Returns (caller frees) a report with the same field
/// names as `POST /admin/reload`:
/// `{"created":[..],"replaced":[..],"stubPatched":[..],"deleted":[..],"failed":[{"port":0,"error":".."}]}`.
/// Returns null only on invalid input / up-front validation failure — then nothing was mutated
/// and the reason is in [`rift_last_error`]. Partial per-port failures come back in `failed`.
///
/// # Safety
/// `h` must be a live handle and `config_json` a valid C string (or null).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_apply_config(
    h: *mut RiftHandle,
    config_json: *const c_char,
) -> *mut c_char {
    ffi_guard!("rift_apply_config", std::ptr::null_mut(), unsafe {
        clear_last_error();
        let (Some(handle), Some(s)) = (handle(h), c_str(config_json)) else {
            set_last_error("rift_apply_config: null handle or config pointer");
            return std::ptr::null_mut();
        };
        let value: Value = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(e) => {
                set_last_error(format!("rift_apply_config: invalid JSON: {e}"));
                return std::ptr::null_mut();
            }
        };
        let configs = match parse_imposter_configs(&value) {
            Ok(c) => c,
            Err(e) => {
                set_last_error(format!("rift_apply_config: {e}"));
                return std::ptr::null_mut();
            }
        };
        match handle
            .runtime
            .block_on(handle.manager.apply_config(configs))
        {
            Ok(report) => {
                let failed: Vec<Value> = report
                    .failed
                    .iter()
                    .map(|(port, e)| json!({"port": port, "error": e.to_string()}))
                    .collect();
                let out = json!({
                    "created": report.created,
                    "replaced": report.replaced,
                    "stubPatched": report.stub_patched,
                    "deleted": report.deleted,
                    "failed": failed,
                });
                into_c_string(out.to_string())
            }
            Err(e) => {
                set_last_error(format!("rift_apply_config: {e}"));
                warn!(error = %e, "rift_apply_config validation failed");
                std::ptr::null_mut()
            }
        }
    })
}

/// Build identity as a STATIC JSON string — never freed; probe this symbol to detect a v2 library
/// (issue #343). `{"version":"..","commit":"<sha>|null","builtAt":"<iso8601>|null","features":[..]}`.
/// `commit`/`builtAt` are `null` unless stamped at build time (issue #344).
#[unsafe(no_mangle)]
pub extern "C" fn rift_build_info() -> *const c_char {
    static INFO: OnceLock<CString> = OnceLock::new();
    INFO.get_or_init(|| {
        let mut features: Vec<&str> = Vec::new();
        if cfg!(feature = "redis-backend") {
            features.push("redis-backend");
        }
        if cfg!(feature = "javascript") {
            features.push("javascript");
        }
        let info = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "commit": option_env!("RIFT_COMMIT"),
            "builtAt": option_env!("RIFT_BUILT_AT"),
            "features": features,
        });
        CString::new(info.to_string()).unwrap_or_else(|_| CString::new("{}").expect("no NUL"))
    })
    .as_ptr()
}

/// Take this thread's last-error message (set by a failed `rift_*` call), or null if none.
/// Reading it clears it; the caller frees the returned string with [`rift_free`] (issue #343).
#[unsafe(no_mangle)]
pub extern "C" fn rift_last_error() -> *mut c_char {
    LAST_ERROR.with(|e| match e.borrow_mut().take() {
        Some(c) => c.into_raw(),
        None => std::ptr::null_mut(),
    })
}

/// Free a string previously returned by a `rift-ffi` function. Null is a no-op.
///
/// # Safety
/// `p` must be null or a pointer returned by a `rift-ffi` function (never [`rift_build_info`],
/// whose static string must not be freed) and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_free(p: *mut c_char) {
    unsafe {
        if !p.is_null() {
            drop(CString::from_raw(p));
        }
    }
}

/// Stop the engine: shut down the admin/metrics listeners first, then gracefully shut down all
/// imposters, and drop the handle + runtime. Null is a no-op. The handle must not be used after.
///
/// # Safety
/// `h` must be null or a pointer returned by [`rift_start`] and not previously stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rift_stop(h: *mut RiftHandle) {
    ffi_guard!("rift_stop", (), unsafe {
        clear_last_error();
        if h.is_null() {
            return;
        }
        let handle = Box::from_raw(h);
        // Ordering (issue #343/#410/#493): admin/metrics + intercept listeners down first, then the
        // manager.
        let plane = handle.admin.lock().take();
        handle.runtime.block_on(async {
            if let Some(plane) = plane {
                plane.admin.shutdown().await;
                if let Some(metrics) = plane.metrics {
                    metrics.shutdown().await;
                }
            }
            handle.intercept.stop().await;
            handle.manager.shutdown().await;
        });
    })
}

#[cfg(test)]
mod panic_safety_tests {
    use super::*;

    // Issue #484: a panic inside an FFI body must be caught at the boundary and turned into the
    // function's sentinel + a recorded last-error — never unwound across the C ABI.
    #[test]
    fn ffi_guard_catches_panic_sets_error_returns_sentinel() {
        clear_last_error();
        let r: i32 = ffi_guard!("panic_probe", -1i32, { panic!("boom-{}", 42) });
        assert_eq!(r, -1, "a caught panic must return the sentinel");

        let err = rift_last_error();
        assert!(!err.is_null(), "a caught panic must record last_error");
        let msg = unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() };
        unsafe { rift_free(err) };
        assert!(
            msg.contains("boom-42"),
            "last_error must carry the panic message: {msg}"
        );
        assert!(
            msg.contains("panic_probe"),
            "last_error must name the function: {msg}"
        );
    }

    // The guard must be transparent on the success path (returns the body's value unchanged).
    #[test]
    fn ffi_guard_passes_through_on_success() {
        let v: u16 = ffi_guard!("ok_probe", 0u16, { 4545u16 });
        assert_eq!(v, 4545);
    }

    // The pure accessors / deallocator are deliberately NOT wrapped in `ffi_guard!` (issue #484):
    // wrapping would call `set_last_error` on panic, breaking the documented contract that
    // `rift_build_info` and `rift_free` leave a pending last-error untouched (so "check sentinel →
    // read rift_last_error → rift_free" is order-independent). Pin that contract so a future edit
    // that accidentally clears last_error in one of these is caught.
    #[test]
    fn pure_accessors_do_not_clobber_last_error() {
        clear_last_error();
        set_last_error("pending-error");
        let _ = rift_build_info();
        unsafe { rift_free(std::ptr::null_mut()) };
        let err = rift_last_error();
        assert!(
            !err.is_null(),
            "an accessor/free must not clear a pending last_error"
        );
        let msg = unsafe { CStr::from_ptr(err).to_string_lossy().into_owned() };
        unsafe { rift_free(err) };
        assert_eq!(msg, "pending-error");
    }
}

#[cfg(test)]
mod serve_options_tests {
    use super::*;

    // Issue #492: rift_serve_admin gains an `allowInjection` option (camelCase) that gates
    // script imposters on the embedded admin plane. It must parse to the typed field and default
    // to None (→ false at the call site) when absent, preserving prior behavior.
    #[test]
    fn serve_options_parses_allow_injection() {
        let on: ServeOptions =
            serde_json::from_str(r#"{"allowInjection": true}"#).expect("parse allowInjection");
        assert_eq!(on.allow_injection, Some(true));

        let off: ServeOptions =
            serde_json::from_str(r#"{"allowInjection": false}"#).expect("parse allowInjection");
        assert_eq!(off.allow_injection, Some(false));

        let absent: ServeOptions = serde_json::from_str("{}").expect("parse empty options");
        assert_eq!(
            absent.allow_injection, None,
            "absent allowInjection must default to None (false at the call site)"
        );

        // A wrong-type value is a surfaced serde error, not a silent coercion (like the other opts).
        assert!(
            serde_json::from_str::<ServeOptions>(r#"{"allowInjection": "yes"}"#).is_err(),
            "a non-bool allowInjection must be a parse error, not silently coerced"
        );
    }
}
