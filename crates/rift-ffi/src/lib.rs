//! Rift C-ABI (issue #204): a tiny opaque-handle + JSON in / JSON out FFI over the rift-core
//! engine, so a host (e.g. the JVM via Panama FFM) can embed Rift in-process.
//!
//! Boundary discipline:
//! - **Opaque handle + JSON only.** No Rust enums/generics cross the line; the JSON codec is the
//!   same `rift-types`/`rift-core` wire model the admin API uses, so it is version-tolerant.
//! - **Memory created and freed on the same side.** `rift_start`/`rift_stop` own the
//!   [`RiftHandle`]; `rift_recorded` returns a `*mut c_char` the caller must hand back to
//!   [`rift_free`].
//! - **No futures cross the boundary.** The handle owns a multi-thread Tokio runtime; mutating
//!   downcalls are blocking `block_on` calls (read-only ones like [`rift_recorded`] are plain
//!   synchronous reads), so the host wraps them in its own blocking effect.
//! - **One handle is safe to share across host threads:** the engine is `Sync` and every
//!   downcall takes `&self`, so concurrent calls on the same handle are permitted.
//!
//! All `extern "C"` functions are panic-free; under edition 2021 an unexpected unwind aborts
//! rather than crossing the boundary (defined behaviour). A failed downcall returns its sentinel
//! and emits a `tracing` event with the dropped error so the reason is not lost.

use rift_core::imposter::{ImposterConfig, ImposterManager, Stub};
use std::ffi::{c_char, CStr, CString};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tracing::warn;

/// Opaque handle: a Tokio runtime (on its own threads) plus the engine it drives.
pub struct RiftHandle {
    runtime: Runtime,
    manager: Arc<ImposterManager>,
}

/// Borrow the handle from a raw pointer, or `None` if null.
///
/// # Safety
/// `h` must be null or a pointer returned by [`rift_start`] and not yet passed to [`rift_stop`].
unsafe fn handle<'a>(h: *mut RiftHandle) -> Option<&'a RiftHandle> {
    h.as_ref()
}

/// Read a borrowed UTF-8 string from a C string pointer, or `None` if null/invalid.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated C string that outlives the borrow.
unsafe fn c_str<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// Move a `String` across the boundary as an owned `*mut c_char` the caller frees with
/// [`rift_free`]. Returns null if the string contains an interior NUL.
fn into_c_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Start the engine. Returns an opaque handle, or null if the runtime could not be created.
#[no_mangle]
pub extern "C" fn rift_start() -> *mut RiftHandle {
    match Runtime::new() {
        Ok(runtime) => Box::into_raw(Box::new(RiftHandle {
            runtime,
            manager: Arc::new(ImposterManager::new()),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Create an imposter from a JSON config. Returns its port, or `0` on any error
/// (null handle/json, malformed config, or bind failure — `0` is never a live imposter port).
///
/// # Safety
/// `h` must be a live handle and `json` a valid C string (or null).
#[no_mangle]
pub unsafe extern "C" fn rift_create_imposter(h: *mut RiftHandle, json: *const c_char) -> u16 {
    let (Some(handle), Some(s)) = (handle(h), c_str(json)) else {
        warn!("rift_create_imposter: null handle or config pointer");
        return 0;
    };
    let config = match serde_json::from_str::<ImposterConfig>(s) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "rift_create_imposter: invalid config JSON");
            return 0;
        }
    };
    handle
        .runtime
        .block_on(handle.manager.create_imposter(config))
        .unwrap_or_else(|e| {
            warn!(error = %e, "rift_create_imposter failed");
            0
        })
}

/// Replace all stubs on `port` from a JSON array. Returns `0` on success, `-1` on any error.
///
/// # Safety
/// `h` must be a live handle and `json` a valid C string (or null).
#[no_mangle]
pub unsafe extern "C" fn rift_replace_stubs(
    h: *mut RiftHandle,
    port: u16,
    json: *const c_char,
) -> i32 {
    let (Some(handle), Some(s)) = (handle(h), c_str(json)) else {
        warn!("rift_replace_stubs: null handle or stubs pointer");
        return -1;
    };
    let stubs = match serde_json::from_str::<Vec<Stub>>(s) {
        Ok(v) => v,
        Err(e) => {
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
            warn!(error = %e, port, "rift_replace_stubs failed");
            -1
        }
    }
}

/// Remove all imposters. Returns `0` on success, `-1` if the handle is null.
///
/// # Safety
/// `h` must be a live handle (or null).
#[no_mangle]
pub unsafe extern "C" fn rift_delete_all(h: *mut RiftHandle) -> i32 {
    let Some(handle) = handle(h) else {
        return -1;
    };
    handle.runtime.block_on(handle.manager.delete_all());
    0
}

/// Return the recorded requests for `port` as a JSON array string the caller must free with
/// [`rift_free`]. Returns null on any error (null/unknown handle or port, or encode failure).
///
/// # Safety
/// `h` must be a live handle (or null).
#[no_mangle]
pub unsafe extern "C" fn rift_recorded(h: *mut RiftHandle, port: u16) -> *mut c_char {
    let Some(handle) = handle(h) else {
        return std::ptr::null_mut();
    };
    let imposter = match handle.manager.get_imposter(port) {
        Ok(i) => i,
        Err(e) => {
            warn!(error = %e, port, "rift_recorded: no such imposter");
            return std::ptr::null_mut();
        }
    };
    match serde_json::to_string(&imposter.get_recorded_requests()) {
        Ok(json) => into_c_string(json),
        Err(e) => {
            warn!(error = %e, port, "rift_recorded: failed to encode recorded requests");
            std::ptr::null_mut()
        }
    }
}

/// Free a string previously returned by [`rift_recorded`]. Null is a no-op.
///
/// # Safety
/// `p` must be null or a pointer returned by a `rift-ffi` function and not yet freed.
#[no_mangle]
pub unsafe extern "C" fn rift_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// Stop the engine: gracefully shut down all imposters and drop the handle + runtime. Null is a
/// no-op. The handle must not be used after this call.
///
/// # Safety
/// `h` must be null or a pointer returned by [`rift_start`] and not previously stopped.
#[no_mangle]
pub unsafe extern "C" fn rift_stop(h: *mut RiftHandle) {
    if h.is_null() {
        return;
    }
    let handle = Box::from_raw(h);
    handle.runtime.block_on(handle.manager.shutdown());
}
