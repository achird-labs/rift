//! A deliberately failing flow store, installed through the `FlowStoreProvider` seam
//! (issue #1463; it replaced the `test-backend` feature's `"failing"` backend).
//!
//! Every op annotates itself via `decorate::annotate`, then fails with a `BackendUnavailable`
//! source, so backend-outage paths run without a real unreachable backend (issues #318, #358).

use rift_http_proxy::extensions::decorate::{BackendUnavailable, annotate};
use rift_http_proxy::extensions::flow_state::{FlowStore, FlowStoreProvider};
use rift_http_proxy::imposter::{ImposterConfig, ImposterManager};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug)]
pub struct FailingFlowStore;

impl FailingFlowStore {
    fn fail<T>(&self, op: &'static str, flow_id: &str, key: &str) -> anyhow::Result<T> {
        annotate(op, format!("{flow_id}/{key}"));
        Err(anyhow::Error::new(BackendUnavailable {
            feature: "flowState",
            detail: format!("failing test backend: {op} {flow_id}/{key}"),
        }))
    }
}

impl FlowStore for FailingFlowStore {
    fn get(&self, flow_id: &str, key: &str) -> anyhow::Result<Option<Value>> {
        self.fail("flowStore.get", flow_id, key)
    }
    fn set(&self, flow_id: &str, key: &str, _value: Value) -> anyhow::Result<()> {
        self.fail("flowStore.set", flow_id, key)
    }
    fn exists(&self, flow_id: &str, key: &str) -> anyhow::Result<bool> {
        self.fail("flowStore.exists", flow_id, key)
    }
    fn delete(&self, flow_id: &str, key: &str) -> anyhow::Result<()> {
        self.fail("flowStore.delete", flow_id, key)
    }
    fn increment(&self, flow_id: &str, key: &str) -> anyhow::Result<i64> {
        self.fail("flowStore.increment", flow_id, key)
    }
    fn set_ttl(&self, flow_id: &str, _ttl_seconds: i64) -> anyhow::Result<()> {
        self.fail("flowStore.setTtl", flow_id, "")
    }
    fn set_key_ttl(&self, flow_id: &str, key: &str, _ttl_seconds: i64) -> anyhow::Result<bool> {
        self.fail("flowStore.setKeyTtl", flow_id, key)
    }
    fn clear_flow(&self, flow_id: &str) -> anyhow::Result<()> {
        self.fail("flowStore.clearFlow", flow_id, "")
    }
}

/// Hands every imposter the failing store, whatever its `_rift.flowState` says.
pub struct FailingFlowStoreProvider;

impl FlowStoreProvider for FailingFlowStoreProvider {
    fn provide(&self, _config: &ImposterConfig) -> Option<Arc<dyn FlowStore>> {
        Some(Arc::new(FailingFlowStore))
    }
}

/// A manager whose every imposter gets the failing store.
pub fn failing_manager() -> ImposterManager {
    ImposterManager::new().with_flow_store_provider(Arc::new(FailingFlowStoreProvider))
}
