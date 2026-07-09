//! Rift C-ABI (issue #204, extended in #343): a tiny opaque-handle + JSON in / JSON out FFI over
//! the rift-core engine, so a host (e.g. the JVM via Panama FFM) can embed Rift in-process.
//!
//! Boundary discipline:
//! - **Opaque handle + JSON only.** No Rust enums/generics cross the line; the JSON codec is the
//!   same `rift-types`/`rift-core` wire model the admin API uses, so it is version-tolerant.
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
use rift_core::imposter::{ApplyReport, ImposterConfig, ImposterManager, Stub};
use rift_core::proxy::intercept_ca::{CertificateAuthority, SniCertResolver};
use rift_core::proxy::truststore::{TrustStorePassword, ca_pem, export_jks, export_pkcs12};
use rift_http_proxy::admin_api::{AdminApiServer, RunningAdminApi};
use rift_http_proxy::config_loader::{self, ConfigSource};
use rift_http_proxy::intercept::InterceptListener;
use rift_http_proxy::intercept_rules::{InterceptRule, InterceptRules, InterceptState};
use rift_http_proxy::server::{RunningMetrics, bind_metrics_server};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    intercept: Mutex<Option<InterceptPlane>>,
}

/// The admin API (and optional metrics server) serving in-process for a handle (issue #343).
struct AdminPlane {
    admin: RunningAdminApi,
    metrics: Option<RunningMetrics>,
}

/// The intercept/TLS-MITM listener serving in-process for a handle, plus the shared
/// [`InterceptState`] (rule store + CA) its control-plane functions mutate/export (issue #410).
struct InterceptPlane {
    listener: InterceptListener,
    state: InterceptState,
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
                intercept: Mutex::new(None),
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

/// `rift_start_intercept` options (all optional): the bind host and port, plus an optional
/// caller-provided intercept CA (`caCertPath`/`caKeyPath`, PEM file paths — both or neither).
/// `deny_unknown_fields` so a misspelled key (e.g. `caCertpath`) is a hard error, not a silent
/// fallback to a fresh ephemeral CA that would defeat the caller's intended CA reuse.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InterceptOptions {
    host: Option<String>,
    port: Option<u16>,
    ca_cert_path: Option<String>,
    ca_key_path: Option<String>,
}

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
        let opts: InterceptOptions = if options_json.is_null() {
            InterceptOptions::default()
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

        // Hold the slot across build+set so a concurrent call can't race two listeners into
        // existence (one intercept listener per handle).
        let mut slot = handle.intercept.lock();
        if slot.is_some() {
            set_last_error(
                "rift_start_intercept: already started (one intercept listener per handle)",
            );
            return std::ptr::null_mut();
        }

        let host = opts.host.as_deref().unwrap_or("127.0.0.1");
        let addr: std::net::SocketAddr = match format!("{host}:{}", opts.port.unwrap_or(0)).parse()
        {
            Ok(a) => a,
            Err(e) => {
                set_last_error(format!("rift_start_intercept: invalid host/port: {e}"));
                return std::ptr::null_mut();
            }
        };
        let ca = match CertificateAuthority::load_or_generate(
            opts.ca_cert_path.as_deref().map(Path::new),
            opts.ca_key_path.as_deref().map(Path::new),
        ) {
            Ok(ca) => Arc::new(ca),
            Err(e) => {
                // `{e:#}` so the chained cause (missing file, bad PEM, mismatched pair) reaches the
                // caller — `rift_last_error` is their only diagnostic channel.
                set_last_error(format!("rift_start_intercept: CA setup failed: {e:#}"));
                warn!(error = %e, "rift_start_intercept: CA setup failed");
                return std::ptr::null_mut();
            }
        };
        let rules = InterceptRules::new();
        let resolver = Arc::new(SniCertResolver::new(ca.clone()));
        let listener =
            match handle
                .runtime
                .block_on(InterceptListener::bind(addr, resolver, rules.clone()))
            {
                Ok(l) => l,
                Err(e) => {
                    set_last_error(format!("rift_start_intercept: bind failed: {e}"));
                    warn!(error = %e, "rift_start_intercept: bind failed");
                    return std::ptr::null_mut();
                }
            };
        // Derive both fields from the real bound address so the URL reflects the actual host
        // (and OS-assigned port), not a hardcoded loopback.
        let local_addr = listener.local_addr();
        let response = json!({
            "interceptPort": local_addr.port(),
            "interceptUrl": format!("http://{local_addr}"),
        })
        .to_string();
        *slot = Some(InterceptPlane {
            listener,
            state: InterceptState { rules, ca },
        });
        into_c_string(response)
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
        let slot = handle.intercept.lock();
        let Some(plane) = slot.as_ref() else {
            set_last_error("rift_intercept_add_rules: intercept not started");
            return -1;
        };
        match parsed {
            RuleOrRules::One(rule) => plane.state.rules.add(rule),
            RuleOrRules::Many(rules) => {
                for rule in rules {
                    plane.state.rules.add(rule);
                }
            }
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
        let slot = handle.intercept.lock();
        let Some(plane) = slot.as_ref() else {
            set_last_error("rift_intercept_clear_rules: intercept not started");
            return -1;
        };
        plane.state.rules.clear();
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
        let slot = handle.intercept.lock();
        let Some(plane) = slot.as_ref() else {
            set_last_error("rift_intercept_list_rules: intercept not started");
            return std::ptr::null_mut();
        };
        match serde_json::to_string(&plane.state.rules.list()) {
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
        let slot = handle.intercept.lock();
        let Some(plane) = slot.as_ref() else {
            set_last_error("rift_intercept_ca_pem: intercept not started");
            return std::ptr::null_mut();
        };
        into_c_string(ca_pem(&plane.state.ca))
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
        let slot = handle.intercept.lock();
        let Some(plane) = slot.as_ref() else {
            set_last_error("rift_intercept_export_truststore: intercept not started");
            return -1;
        };
        let pw = TrustStorePassword::new(password);
        let bytes = match format {
            "pkcs12" => export_pkcs12(&plane.state.ca, &pw),
            "jks" => export_jks(&plane.state.ca, &pw),
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

    let mut server = AdminApiServer::new(addr, Arc::clone(&handle.manager), api_key);
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
        // Ordering (issue #343/#410): admin/metrics + intercept listeners down first, then the
        // manager.
        let plane = handle.admin.lock().take();
        let intercept = handle.intercept.lock().take();
        handle.runtime.block_on(async {
            if let Some(plane) = plane {
                plane.admin.shutdown().await;
                if let Some(metrics) = plane.metrics {
                    metrics.shutdown().await;
                }
            }
            if let Some(intercept) = intercept {
                intercept.listener.shutdown().await;
            }
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
