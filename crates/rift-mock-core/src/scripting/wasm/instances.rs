//! `instance: "imposter"` (issue #1793): one live instance per component an imposter's stubs
//! reference, kept across requests so its guest memory is its state.
//!
//! **Identity is the component, not the stub.** An instance is keyed by the component's SHA-256,
//! its `export` as written and its `knobs` as canonical JSON: two stubs with the same three share
//! one instance, and editing a stub's predicates, its `_rift.fault` or its place in the list keeps
//! the instance. Only a stub edit that leaves no stub referencing a key drops it.
//!
//! **A call is detached from its request.** It runs on a task of its own that holds the instance,
//! so a client that hangs up, or a delete that cuts the request, does not cut a guest half-way
//! through a change to its own memory. The call is still bounded by `timeoutMs`.
//!
//! **Take on call, return on success.** A call takes the instance out of its slot and puts it
//! back only after a clean return, whatever the guest answered: a trap, a deadline miss, a panic
//! or an abort leaves the slot empty, and the next call instantiates afresh.

use super::runtime::{CallReport, CallSpec, Live, SnapshotFailure};
use crate::extensions::flow_state::FlowStore;
use crate::imposter::WasmLimits;
use crate::scripting::{WasmAnswer, WasmInstanceInfo, WasmSnapshot, WasmSnapshotError};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

/// The least deadline a call on a kept instance starts with: two epoch ticks.
const MIN_CALL_TIME: std::time::Duration = std::time::Duration::from_millis(20);

/// What identifies a kept instance (issue #1793).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct InstanceKey {
    sha256: [u8; 32],
    /// `_rift.script.export` as written: `None`, `"rift"` and `"http"` are three keys.
    export: Option<String>,
    /// `_rift.script.knobs` as JSON text with sorted object keys; `{}` when absent.
    knobs: String,
}

impl InstanceKey {
    pub(crate) fn new(
        sha256: [u8; 32],
        export: Option<&str>,
        knobs: Option<&serde_json::Value>,
    ) -> Self {
        Self {
            sha256,
            export: export.map(str::to_owned),
            // `serde_json` without `preserve_order` keeps object keys sorted, so equal knobs
            // serialize alike whatever order they were written in.
            knobs: knobs.map_or_else(|| "{}".to_owned(), serde_json::Value::to_string),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The key's id in `GET /imposters/:port/wasm/instances`: the SHA-256 of the three parts.
    fn id(&self) -> String {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        ctx.update(&self.sha256);
        ctx.update(&[0]);
        ctx.update(self.export.as_deref().unwrap_or("").as_bytes());
        ctx.update(&[0]);
        ctx.update(self.knobs.as_bytes());
        Self::hex(ctx.finish().as_ref())
    }

    /// `wasi:random/insecure-seed` for the instance, from its `id`: the same every run and for
    /// every request order.
    pub(super) fn insecure_seed(&self) -> u128 {
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        ctx.update(b"rift wasm insecure-seed");
        ctx.update(self.id().as_bytes());
        let digest = ctx.finish();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.as_ref()[..16]);
        u128::from_le_bytes(bytes)
    }

    fn knobs_hash(&self) -> String {
        Self::hex(&ring::digest::digest(&ring::digest::SHA256, self.knobs.as_bytes()).as_ref()[..6])
    }
}

/// The key a stub's script keeps its instance under, when it is `instance: "imposter"`: the
/// component's SHA-256 from its `component` reference, or from its `code` (the cached compile's,
/// else the bytes hashed now). `None` for any other script, and for one whose component cannot be
/// named (a malformed reference, `code` that is not base64), which keeps no instance either.
pub(crate) fn key_of(script: &crate::imposter::RiftScriptConfig) -> Option<InstanceKey> {
    if script.instance.as_deref() != Some("imposter") {
        return None;
    }
    let sha256 = match (script.component.as_deref(), script.code.as_deref()) {
        (Some(reference), _) => reference
            .parse::<crate::imposter::components::ComponentId>()
            .ok()?
            .bytes(),
        (None, Some(code)) => super::runtime::code_sha256(code)?,
        (None, None) => return None,
    };
    Some(InstanceKey::new(
        sha256,
        script.export.as_deref(),
        script.knobs.as_ref(),
    ))
}

/// One key's instance, and what the admin route reports about it.
struct Slot {
    live: Arc<tokio::sync::Mutex<Option<Live>>>,
    meta: parking_lot::Mutex<Meta>,
}

impl Slot {
    /// Record that the slot holds no instance: after a drop, or a call task that panicked holding
    /// it, so the route does not list an instance that is gone.
    fn forget(&self) {
        let mut meta = self.meta.lock();
        meta.instantiated_at = None;
        meta.calls = 0;
        meta.memory_bytes = 0;
        meta.bounds = None;
    }
}

#[derive(Default)]
struct Meta {
    instantiated_at: Option<SystemTime>,
    calls: u64,
    memory_bytes: usize,
    /// The limits and `timeoutMs` of the last call: a snapshot runs under them (issue #1794).
    bounds: Option<(WasmLimits, std::time::Duration)>,
}

/// An imposter's kept instances.
#[derive(Default)]
pub(crate) struct InstanceMap {
    slots: parking_lot::Mutex<HashMap<InstanceKey, Arc<Slot>>>,
}

/// Counts a kept instance in `rift_wasm_instances` while it lives.
#[derive(Debug)]
pub(crate) struct Counted;

impl Counted {
    fn new() -> Self {
        crate::extensions::metrics::wasm_instances_gauge(1);
        Self
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        crate::extensions::metrics::wasm_instances_gauge(-1);
    }
}

/// Why an instance was dropped, as its `warn` line and `rift_wasm_instance_drops_total` say.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DropReason {
    /// No stub references its key any more.
    Unreferenced,
    /// `restart_runtime_state`.
    Reset,
    /// The call trapped: a trap, fuel, memory.
    Trap,
    /// The call missed its deadline while the guest ran.
    Deadline,
    /// The guest returned with its response body or outparam still open.
    Unfinished,
    /// `rift:mock/introspect.snapshot` trapped or missed its deadline (issue #1794).
    SnapshotTrap,
}

impl DropReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unreferenced => "unreferenced",
            Self::Reset => "reset",
            Self::Trap => "trap",
            Self::Deadline => "deadline",
            Self::Unfinished => "unfinished",
            Self::SnapshotTrap => "snapshot-trap",
        }
    }
}

fn log_drop(key: &InstanceKey, reason: DropReason) {
    crate::extensions::metrics::record_wasm_instance_drop(reason.as_str());
    tracing::warn!(
        target: crate::scripting::trace::SCRIPT_LOG_TARGET,
        engine = "wasm",
        sha = %InstanceKey::hex(&key.sha256[..6]),
        knobs = %key.knobs_hash(),
        reason = reason.as_str(),
        "wasm instance dropped; its guest state is discarded"
    );
}

impl InstanceMap {
    /// Whether no instance is alive: a slot a call made and could not keep holds nothing to prune.
    pub(crate) fn is_empty(&self) -> bool {
        self.slots
            .lock()
            .values()
            .all(|slot| slot.meta.lock().instantiated_at.is_none())
    }

    fn slot(&self, key: &InstanceKey) -> Arc<Slot> {
        let mut slots = self.slots.lock();
        Arc::clone(slots.entry(key.clone()).or_insert_with(|| {
            Arc::new(Slot {
                live: Arc::default(),
                meta: parking_lot::Mutex::default(),
            })
        }))
    }

    /// Keep only the keys `referenced` holds. A dropped key's call in flight keeps its `Arc` and
    /// finishes on the old instance; a request that matched before the edit can insert a removed
    /// key again, and the next edit removes it (accepted: a stub edit and a request racing on the
    /// stub it removes have no order to keep).
    pub(crate) fn retain(&self, referenced: &std::collections::HashSet<InstanceKey>) {
        let mut slots = self.slots.lock();
        slots.retain(|key, slot| {
            let keep = referenced.contains(key);
            if !keep && slot.meta.lock().instantiated_at.is_some() {
                log_drop(key, DropReason::Unreferenced);
            }
            keep
        });
    }

    /// Drop every instance (`restart_runtime_state`).
    pub(crate) fn clear(&self) {
        let mut slots = self.slots.lock();
        for (key, slot) in slots.iter() {
            if slot.meta.lock().instantiated_at.is_some() {
                log_drop(key, DropReason::Reset);
            }
        }
        slots.clear();
    }

    pub(crate) fn list(&self) -> Vec<WasmInstanceInfo> {
        let slots = self.slots.lock();
        let mut out: Vec<WasmInstanceInfo> = slots
            .iter()
            .filter_map(|(key, slot)| {
                let meta = slot.meta.lock();
                let at = meta.instantiated_at?;
                Some(WasmInstanceInfo {
                    id: key.id(),
                    sha256: InstanceKey::hex(&key.sha256),
                    export: key.export.clone(),
                    knobs_hash: key.knobs_hash(),
                    instantiated_at: at
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    calls: meta.calls,
                    memory_bytes: meta.memory_bytes,
                })
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

/// Run `call` on `key`'s instance (issue #1793): wait for it within the call's deadline, then call
/// it on a task of its own, which keeps going if this future is dropped.
pub(super) async fn call_in(
    map: &InstanceMap,
    key: InstanceKey,
    mut call: CallSpec,
) -> (anyhow::Result<WasmAnswer>, CallReport) {
    let mut report = CallReport::default();
    call.insecure_seed = Some(key.insecure_seed());
    let slot = map.slot(&key);
    // Waiting for the instance counts toward `timeoutMs`; a miss here drops nothing.
    let guard =
        match tokio::time::timeout(call.remaining(), Arc::clone(&slot.live).lock_owned()).await {
            Ok(guard) => guard,
            Err(_elapsed) => return (Err(super::runtime::timeout_error_for(&call)), report),
        };
    // A call that got its instance with too little of its deadline left would be interrupted at
    // the next epoch tick and take the instance with it: it times out here instead, dropping
    // nothing, as a call that never got the instance does.
    if call.remaining() < MIN_CALL_TIME {
        drop(guard);
        return (Err(super::runtime::timeout_error_for(&call)), report);
    }
    let watched = Arc::clone(&slot);
    let task = tokio::spawn(async move {
        let mut guard = guard;
        let mut report = CallReport::default();
        let held = guard.take();
        let fresh = held.is_none();
        let bounds = call.bounds();
        let (result, kept) = call.run(held, &mut report).await;
        match kept {
            Some(mut live) => {
                live.release_request();
                if fresh {
                    live.counted = Some(Counted::new());
                }
                let mut meta = slot.meta.lock();
                if fresh {
                    meta.instantiated_at = Some(SystemTime::now());
                    meta.calls = 0;
                }
                meta.calls += 1;
                meta.memory_bytes = live.memory_bytes();
                meta.bounds = Some(bounds);
                drop(meta);
                *guard = Some(live);
            }
            None => {
                // An instance this call made and could not keep was never one the route reported.
                let had = !fresh;
                slot.forget();
                if had {
                    let reason = if report.unfinished {
                        DropReason::Unfinished
                    } else {
                        match report.trap.as_ref().map(|(kind, _)| *kind) {
                            Some(super::runtime::TrapKind::Deadline) => DropReason::Deadline,
                            _ => DropReason::Trap,
                        }
                    };
                    log_drop(&key, reason);
                }
            }
        }
        (result, report)
    });
    match task.await {
        Ok((result, call_report)) => {
            report.absorb(call_report);
            (result, report)
        }
        // A panic dropped the instance with the task's guard.
        Err(e) => {
            watched.forget();
            (
                Err(anyhow::anyhow!("wasm instance task failed: {e}")),
                report,
            )
        }
    }
}

/// Call `rift:mock/introspect.snapshot` on a live instance (issue #1794): `id` as
/// `GET /imposters/:port/wasm/instances` lists it, or the only live one when `None`. Never
/// instantiates, since that would run `init` as a side effect.
///
/// Waits for the instance as a request does, under the last call's `timeoutMs`, so it never sees a
/// call half-applied; then runs on a task of its own, as a call does, so an admin client that hangs
/// up does not cut the guest. Not a call: `calls` does not move. A trap or deadline miss drops the
/// instance, as on the request path.
pub(crate) async fn snapshot(
    map: &InstanceMap,
    id: Option<&str>,
    flow_store: Arc<dyn FlowStore>,
) -> Result<WasmSnapshot, WasmSnapshotError> {
    let (key, slot) = {
        let slots = map.slots.lock();
        let mut live: Vec<(&InstanceKey, &Arc<Slot>)> = slots
            .iter()
            .filter(|(_, slot)| slot.meta.lock().instantiated_at.is_some())
            .collect();
        live.sort_by_cached_key(|(key, _)| key.id());
        let chosen = match id {
            Some(id) => live.into_iter().find(|(key, _)| key.id() == id),
            None if live.len() > 1 => {
                return Err(WasmSnapshotError::Ambiguous(
                    live.iter().map(|(key, _)| key.id()).collect(),
                ));
            }
            None => live.into_iter().next(),
        };
        let (key, slot) = chosen.ok_or(WasmSnapshotError::NotInstantiated)?;
        (key.clone(), Arc::clone(slot))
    };
    let (limits, timeout) = slot
        .meta
        .lock()
        .bounds
        .ok_or(WasmSnapshotError::NotInstantiated)?;
    let started = std::time::Instant::now();
    let guard = tokio::time::timeout(timeout, Arc::clone(&slot.live).lock_owned())
        .await
        .map_err(|_elapsed| WasmSnapshotError::Busy)?;
    let watched = Arc::clone(&slot);
    let task = tokio::spawn(async move {
        let mut guard = guard;
        // Dropped by a trap while this waited.
        let Some(mut live) = guard.take() else {
            return Err(WasmSnapshotError::NotInstantiated);
        };
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining < MIN_CALL_TIME {
            *guard = Some(live);
            return Err(WasmSnapshotError::Busy);
        }
        let (result, fuel_used, kept) = live.snapshot(&limits, remaining, flow_store).await;
        if kept {
            live.release_request();
            slot.meta.lock().memory_bytes = live.memory_bytes();
            *guard = Some(live);
        } else {
            drop(live);
            slot.forget();
            log_drop(&key, DropReason::SnapshotTrap);
        }
        drop(guard);
        match result {
            Ok(body) => {
                if serde_json::from_str::<serde::de::IgnoredAny>(&body).is_err() {
                    return Err(WasmSnapshotError::NotJson);
                }
                Ok(WasmSnapshot { body, fuel_used })
            }
            Err(SnapshotFailure::NoExport) => Err(WasmSnapshotError::NoExport),
            Err(SnapshotFailure::Guest(msg)) => Err(WasmSnapshotError::Guest(msg)),
            Err(SnapshotFailure::Trap(msg)) => Err(WasmSnapshotError::Trap(msg)),
        }
    });
    task.await.unwrap_or_else(|e| {
        // A panic dropped the instance with the task's guard.
        watched.forget();
        Err(WasmSnapshotError::Trap(format!(
            "snapshot task failed: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // A snapshot waits for the instance under the last call's `timeoutMs`, as a request does: held
    // past it, the snapshot answers busy and leaves the instance as it was.
    #[tokio::test]
    async fn a_snapshot_that_cannot_get_the_instance_in_time_is_busy() {
        let map = InstanceMap::default();
        let key = InstanceKey::new([7; 32], None, None);
        let slot = map.slot(&key);
        {
            let mut meta = slot.meta.lock();
            meta.instantiated_at = Some(SystemTime::now());
            meta.bounds = Some((WasmLimits::default(), Duration::from_millis(50)));
        }
        let held = Arc::clone(&slot.live).lock_owned().await;
        let started = Instant::now();
        let got = snapshot(
            &map,
            None,
            Arc::new(crate::extensions::flow_state::NoOpFlowStore),
        )
        .await;
        assert_eq!(got, Err(WasmSnapshotError::Busy));
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(
            slot.meta.lock().instantiated_at.is_some(),
            "nothing dropped"
        );
        drop(held);
    }
}
