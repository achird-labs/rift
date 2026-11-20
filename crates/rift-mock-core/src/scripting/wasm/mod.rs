//! `engine: "wasm"` for `_rift.script` (issue #1751): a WebAssembly component built for the
//! `rift:mock` world (`wit/rift-mock.wit`), run under wasmtime with pre-emptive, per-call limits.
//!
//! A component's bytes travel in `_rift.script.code` as base64: resolution reads a `.wasm` `file`
//! into it, as it reads a Rhai or JavaScript file into text, so a persisted or saved imposter
//! carries its component and replays without the file.
//!
//! The sandbox is deny by implementation. A wasm32-wasip2 toolchain imports wasi:cli, wasi:io,
//! wasi:filesystem, wasi:clocks and wasi:random even when the program calls none of them, so those
//! are linked with nothing behind them: no preopened directory, no environment, no arguments, and
//! stdout/stderr captured as the script's log. wasi:sockets and wasi:http are never linked, so a
//! component importing them fails to instantiate and is refused at admission.

mod host;
mod pipe;
mod runtime;

pub use runtime::bench;
pub(crate) use runtime::{admit, admit_imports, emit_logs, respond};

#[cfg(test)]
mod tests;

/// Why a component is refused at admission, or cannot be loaded when served.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WasmAdmitError {
    /// `code` is not base64: an inline source, which this engine has no form for.
    #[error("engine \"wasm\" has no inline form; use file ({0})")]
    NotBase64(String),
    /// The bytes are a core module or not wasm at all.
    #[error("not a component; build for wasm32-wasip2 ({0})")]
    NotComponent(String),
    /// The component imports an interface Rift does not provide.
    #[error(
        "wasm component imports {0}, which Rift does not provide (allowed: wasi:cli, wasi:io, \
         wasi:filesystem, wasi:clocks, wasi:random, rift:mock/state)"
    )]
    Import(String),
    /// The component does not export `rift:mock/handler` with the expected shape, or an allowed
    /// import does not have the shape Rift provides.
    #[error("wasm component does not implement the rift:mock world: {0}")]
    World(String),
    /// `export` names something other than `rift`.
    #[error("wasm export \"{0}\" is not supported; the only export is \"rift\"")]
    Export(String),
    /// The engine itself could not start.
    #[error("{0}")]
    Unavailable(String),
}
