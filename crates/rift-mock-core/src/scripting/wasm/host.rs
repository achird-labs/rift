//! The host side of the `rift:mock` world: a store's state, and the `state` import backed by the
//! imposter's [`FlowStore`].

use crate::extensions::flow_state::{CasOutcome as StoreCas, FlowStore, flow_result};
use std::sync::Arc;
use wasmtime::component::ResourceTable;
use wasmtime::{ResourceLimiter, StoreLimits};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub(super) mod bindings {
    wasmtime::component::bindgen!({
        world: "mock",
        path: "wit",
        imports: { "rift:mock/state": async | trappable, default: trappable },
        exports: { default: async },
    });
}

use bindings::rift::mock::state::{CasOutcome, Host as StateHost};

/// A store's limits. `maxMemoryMiB` caps the sum of every linear memory the call creates, not each
/// one: a component is several core instances, and a module may declare many memories. A refusal
/// is remembered, which is how a call failing on the cap is told from any other trap.
pub(super) struct Limits {
    /// Instances, tables and table elements; its memory size is unbounded, as `memory_cap` is ours.
    pub(super) inner: StoreLimits,
    pub(super) memory_cap: usize,
    pub(super) memory_total: usize,
    pub(super) memory_refused: bool,
}

impl ResourceLimiter for Limits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let total = self
            .memory_total
            .saturating_sub(current)
            .saturating_add(desired);
        if total > self.memory_cap {
            self.memory_refused = true;
            return Err(wasmtime::Error::msg(format!(
                "growing memory to {desired} bytes would bring the call's memories to {total} \
                 bytes, past {} bytes",
                self.memory_cap
            )));
        }
        self.memory_total = total;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.inner.table_growing(current, desired, maximum)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

/// A store's state. In `instance: "request"` mode the store is dropped after its one call; in
/// `"imposter"` mode (issue #1793) it lives as long as its instance, and the fields a call owns —
/// flow, random stream, clock, deadline — are set again before every call.
pub(super) struct Host {
    pub(super) wasi: WasiCtx,
    pub(super) table: ResourceTable,
    /// `wasi:http/types` (issue #1758), for `export: "http"` and any component importing it.
    pub(super) http: wasmtime_wasi_http::WasiHttpCtx,
    pub(super) http_hooks: NoOutgoing,
    pub(super) limits: Limits,
    pub(super) flow_store: Arc<dyn FlowStore>,
    /// The request's flow id: what `flow = none` means, as `ctx.state` is bound to it.
    pub(super) flow_id: String,
    /// The store's own random handle, which `wasi:random` reads; each call points it at the
    /// request's stream (issues #1755, #1795).
    pub(super) rng: crate::imposter::rng::RequestRng,
    /// What `wasi:clocks` read for the current call (issue #1754); `None` is the system clock.
    pub(super) clock: CallClock,
    /// When the current call's deadline passes; `None` is no deadline.
    pub(super) deadline: Option<std::time::Instant>,
}

/// The clock a store's `wasi:clocks` read, set for each call.
pub(super) type CallClock = Arc<parking_lot::RwLock<Option<crate::scripting::ScriptClock>>>;

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl bindings::rift::mock::types::Host for Host {}

impl wasmtime_wasi_http::WasiHttpView for Host {
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

/// The `wasi:http` hooks: no HTTP client, and small host-side body buffers. `wasi:http/outgoing-handler`
/// is never linked, so nothing calls `send_request`; were it ever reached, the request is denied
/// rather than sent (issue #1758).
///
/// A guest can create `wasi:http` bodies whose queued chunks live in host memory, outside
/// `maxMemoryMiB`: each body holds at most [`BODY_BUFFER_CHUNKS`] chunks of [`BODY_CHUNK_BYTES`],
/// and a call at most `MAX_RESOURCES` resources, so the host memory one call can hold this way is
/// bounded (about 64 MiB with the 1024-resource cap, against 2 MiB a body by default).
pub(super) struct NoOutgoing;

/// The most bytes one write to a `wasi:http` body may carry; the library's default is 1 MiB.
pub(super) const BODY_CHUNK_BYTES: usize = 64 * 1024;
/// Writes a body buffers before the writer waits for this side to read (the library's default).
pub(super) const BODY_BUFFER_CHUNKS: usize = 1;
/// The most bytes one `wasi:http` fields resource (a header map) may hold; the default is 128 KiB.
pub(super) const FIELD_SIZE_BYTES: usize = 32 * 1024;

type OutgoingFuture =
    Box<dyn std::future::Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>;

impl wasmtime_wasi_http::WasiHttpHooks for NoOutgoing {
    fn send_request(
        &mut self,
        _request: hyper::Request<wasmtime_wasi_http::WasiBody>,
        _options: Option<wasmtime_wasi_http::RequestOptions>,
        _fut: OutgoingFuture,
    ) -> Box<
        dyn std::future::Future<
                Output = Result<
                    (
                        hyper::Response<wasmtime_wasi_http::WasiBody>,
                        OutgoingFuture,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        Box::new(async { Err(wasmtime_wasi_http::Error::HttpRequestDenied) })
    }

    fn p2_outgoing_body_buffer_chunks(&mut self) -> usize {
        BODY_BUFFER_CHUNKS
    }

    fn p2_outgoing_body_chunk_size(&mut self) -> usize {
        BODY_CHUNK_BYTES
    }
}

/// A guest's JSON text, or a guest error naming the op when it is not JSON.
fn from_text(op: &str, text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|e| format!("{op}: value is not JSON: {e}"))
}

impl Host {
    fn flow(&self, flow: Option<String>) -> String {
        flow.unwrap_or_else(|| self.flow_id.clone())
    }
}

/// Run `op` against `store`, off the request task when the store blocks (issue #475), and render a
/// failure the way `ctx.state` does (`flow_result`). Takes the store by value: a host future must be
/// `Send`, and `&Host` is not.
async fn store_op<T, F>(
    store: Arc<dyn FlowStore>,
    name: &'static str,
    op: F,
) -> wasmtime::Result<Result<T, String>>
where
    T: Send + 'static,
    F: FnOnce(&dyn FlowStore) -> anyhow::Result<T> + Send + 'static,
{
    let result = if store.is_blocking() {
        tokio::task::spawn_blocking(move || op(store.as_ref()))
            .await
            .map_err(|e| wasmtime::Error::msg(format!("flow store task failed: {e}")))?
    } else {
        op(store.as_ref())
    };
    Ok(flow_result(name, result))
}

impl StateHost for Host {
    async fn get(
        &mut self,
        key: String,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<Option<String>, String>> {
        let flow = self.flow(flow);
        let got = store_op(Arc::clone(&self.flow_store), "get", move |s| {
            s.get(&flow, &key)
        })
        .await?;
        Ok(got.map(|v| v.as_ref().map(serde_json::Value::to_string)))
    }

    async fn set(
        &mut self,
        key: String,
        value: String,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<(), String>> {
        let value = match from_text("set", &value) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "set", move |s| {
            s.set(&flow, &key, value)
        })
        .await
    }

    async fn incr_by(
        &mut self,
        key: String,
        by: i64,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<i64, String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "incrementBy", move |s| {
            s.increment_by(&flow, &key, by)
        })
        .await
    }

    async fn exists(
        &mut self,
        key: String,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<bool, String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "exists", move |s| {
            s.exists(&flow, &key)
        })
        .await
    }

    async fn delete(
        &mut self,
        key: String,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<(), String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "delete", move |s| {
            s.delete(&flow, &key)
        })
        .await
    }

    async fn cas(
        &mut self,
        key: String,
        expected: Option<String>,
        new: String,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<CasOutcome, String>> {
        let expected = match expected.as_deref().map(|t| from_text("cas", t)).transpose() {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let new = match from_text("cas", &new) {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        let flow = self.flow(flow);
        let outcome = store_op(Arc::clone(&self.flow_store), "cas", move |s| {
            s.compare_and_set(&flow, &key, expected.as_ref(), new)
        })
        .await?;
        Ok(outcome.map(|o| match o {
            StoreCas::Applied => CasOutcome::Applied,
            StoreCas::Conflict(current) => {
                CasOutcome::Conflict(current.as_ref().map(serde_json::Value::to_string))
            }
        }))
    }

    async fn ttl(
        &mut self,
        seconds: i64,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<(), String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "setTtl", move |s| {
            s.set_ttl(&flow, seconds)
        })
        .await
    }

    async fn key_ttl(
        &mut self,
        key: String,
        seconds: i64,
        flow: Option<String>,
    ) -> wasmtime::Result<Result<bool, String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "setKeyTtl", move |s| {
            s.set_key_ttl(&flow, &key, seconds)
        })
        .await
    }

    async fn clear(&mut self, flow: Option<String>) -> wasmtime::Result<Result<(), String>> {
        let flow = self.flow(flow);
        store_op(Arc::clone(&self.flow_store), "clear", move |s| {
            s.clear_flow(&flow)
        })
        .await
    }
}
