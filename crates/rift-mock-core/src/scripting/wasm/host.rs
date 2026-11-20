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

/// One call's store state. Nothing outlives the call: the store is dropped after it.
pub(super) struct Host {
    pub(super) wasi: WasiCtx,
    pub(super) table: ResourceTable,
    pub(super) limits: Limits,
    pub(super) flow_store: Arc<dyn FlowStore>,
    /// The request's flow id: what `flow = none` means, as `ctx.state` is bound to it.
    pub(super) flow_id: String,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl bindings::rift::mock::types::Host for Host {}

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
