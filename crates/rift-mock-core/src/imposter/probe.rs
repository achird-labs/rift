//! Probe points of the imposter lifecycle, for checking runs of the engine against its TLA+ model
//! (`formal/lifecycle/Lifecycle.tla`; `formal/lifecycle/README.md`, "Trace validation"), and of
//! connection fault injection (`formal/faultio/FaultIo.tla`).
//!
//! Each [`Point`] is one step the model names: the engine calls [`Probe::at`] when it takes that
//! step, in the order it takes them. A test installs a recorder with
//! [`ImposterManager::with_probe`](crate::imposter::ImposterManager::with_probe) and writes the
//! points out; `formal/lifecycle/validate.py` then asks TLC whether the model allows that order.
//! Nothing installs a probe in production: the call site is one `Option` branch, and the points
//! carry `Copy` payloads only, so the cost of an installed probe is the probe's own.
//!
//! A point is where the model's action commits, never before: a point that fires and then fails
//! would record a step the engine did not take.
//!
//! The `Script*` points are the script state cell's (`formal/scripting/StateCell.tla`): they ride
//! the run's `ScriptScope`, because the MB script pool is process-global and belongs to no manager.

use std::sync::Arc;

use super::fault_io::TcpFaultKind;
use crate::recording::ClaimOutcome;

/// A generation: the n-th imposter this manager created, whichever port. Two imposters on one
/// port over time are two generations; the model names them `<<port, n>>`.
pub type Generation = u64;

/// The layer of a connection's stream that took an armed fault (`formal/faultio/FaultIo.tla`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultLayer {
    /// `FaultIo`, on the socket beneath TLS.
    RawSocket,
    /// `TlsPayloadFault`, above TLS.
    AboveTls,
}

/// One step of the lifecycle, as `formal/lifecycle/Lifecycle.tla` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Point {
    /// `bind_for_create` passed its occupancy check and bound the port (`CreateBindOk`).
    CreateBound { port: u16, generation: Generation },
    /// The new generation read the port's load floor (`CreateReadFloor`); `None` when no earlier
    /// generation on the port was deleted.
    CreateFloorRead {
        port: u16,
        generation: Generation,
        floor: Option<i64>,
    },
    /// The accept loops are running but accept nothing until the claim is won (`CreateSpawnAccept`).
    CreateAccepting { port: u16, generation: Generation },
    /// `PortTable::try_claim` stored the generation (`CreateTryClaim`, the winner).
    CreateClaimed { port: u16, generation: Generation },
    /// `PortTable::try_claim` found the slot held; the create fails with `PortInUse`
    /// (`CreateTryClaim`, the loser).
    CreateClaimLost { port: u16, generation: Generation },
    /// The datadir write succeeded, or was skipped for a generation no longer registered
    /// (`CreatePersistWrite`).
    CreatePersisted { port: u16, generation: Generation },
    /// The datadir write failed; the rollback has not run yet (`CreatePersistWrite`, the failing
    /// branch).
    CreatePersistFailed { port: u16, generation: Generation },
    /// The create removed and cancelled its generation after a failed write (`CreateRollback`).
    CreateRolledBack { port: u16, generation: Generation },
    /// A delete took `persist_lock` (`DeleteLock`).
    DeleteLocked { port: u16 },
    /// A delete or replace looked its generation up (`DeleteLookup`, `ReplaceLookup`).
    DeleteLookedUp { port: u16, generation: Generation },
    /// The port has no imposter: the delete returns `NotFound` (`DeleteLookup`,
    /// `DeleteRemoveIfSame`, `ReplaceLookup`).
    DeleteNotFound { port: u16 },
    /// `{port}.json` is gone (`DeleteUnlink`).
    DeleteUnlinked { port: u16, generation: Generation },
    /// The unlink failed; the imposter stays registered and serving (`DeleteUnlink`, the
    /// failing branch).
    DeleteUnlinkFailed { port: u16, generation: Generation },
    /// The generation's clock was folded into the port's floor (`DeleteRecordFloor`).
    DeleteFloorRecorded {
        port: u16,
        generation: Generation,
        floor: i64,
    },
    /// The slot was vacated (`DeleteRemoveIfSame`).
    DeleteRemoved { port: u16, generation: Generation },
    /// The generation's cancel token fired (`DeleteCancel`).
    DeleteCancelled { port: u16, generation: Generation },
    /// The port's shared state was cleared (`DeleteClearShared`).
    DeleteCleared { port: u16, generation: Generation },
    /// The accept loops are joined and the connections drained, or the drain's bound elapsed
    /// (`DeleteDrain`).
    DeleteDrained {
        port: u16,
        generation: Generation,
        timed_out: bool,
    },
    /// The delete returns (`DeleteFinish`).
    DeleteReturned { port: u16, generation: Generation },
    /// An `apply_config` took its `ports()` snapshot; `swept` ports are not desired and will be
    /// deleted (`ApplySnapshot`).
    ApplySwept { swept: usize },
    /// A `delete_all` took its `ports()` snapshot (`ApplySnapshot`).
    DeleteAllSwept { swept: usize },
    /// A response's `Last-Modified` load stamp was produced (`ReqCompute`), in seconds.
    Served {
        port: u16,
        generation: Generation,
        stamp: i64,
    },
    /// A `try_claim_as` answered (`formal/proxyonce/ProxyOnce.tla`: `Claim`, `Reclaim`).
    ProxyClaim { port: u16, outcome: ClaimOutcome },
    /// The recording is shadowed; the request is about to forget it (`ShadowCheck`).
    ProxyShadowed { port: u16 },
    /// The request forgot the shadowed recording (`ShadowForget`).
    ProxyShadowForgot { port: u16 },
    /// An `AlreadyRecorded` is about to look its recording up (before `Lookup`).
    ProxyLookingUp { port: u16 },
    /// The lookup found the recording, which replays (`Lookup`).
    ProxyReplayed { port: u16 },
    /// The lookup found nothing, and the request goes on to forward (`Lookup`).
    ProxyNothingToReplay { port: u16 },
    /// The request is about to call the upstream (`Forward`).
    ProxyForwarding { port: u16 },
    /// `HeldClaim::settle` returned (`Settle`, `SettleFail`). It returns nothing, so this says
    /// neither whether the store failed nor whether it dropped a stale token.
    ProxySettled { port: u16 },
    /// A recorded stub went into the list (`Insert`); `claimed` is whether it carries the claim.
    ProxyInserted { port: u16, claimed: bool },
    /// A stub edit's swap left a claim with no carrier; it is about to be forgotten (`EditSwap`,
    /// `ClearSwap`).
    ProxyOrphanFound { port: u16 },
    /// That claim was forgotten (`EditForget`, `ClearForget`).
    ProxyOrphanForgotten { port: u16 },
    /// `ProxyRecordingStore::clear` ran for the port (`ClearStore`, `RestartClear`).
    ProxyStoreCleared { port: u16 },
    /// A `FaultCell` was armed: the connection's next write performs `kind` (`ServiceDone`).
    FaultArmed { kind: TcpFaultKind },
    /// A write took the armed fault (`FaultIoTrip`, `TlsTakePayload`).
    FaultTaken {
        kind: TcpFaultKind,
        layer: FaultLayer,
    },
    /// A `FaultCell`'s lingering close was asked for (`ServiceDone`).
    LingerArmed,
    /// The lingering close shut the write side: the client sees a FIN (`LingerShutWrite`).
    LingerFinSent,
    /// The lingering close stopped discarding what the client sends (`DrainEnds`).
    LingerDrained { discarded: usize },
    /// A request read the scenario state for the gate of the stub at `stub_index`
    /// (`formal/scenario`: `GateRead`). This and the points below fire only for a scenario, so a
    /// run of an imposter without one records what `formal/lifecycle` covers and nothing else.
    GateRead { port: u16, stub_index: usize },
    /// A gated transition's compare-and-set wrote (`CasCompare`, the applied branch).
    CasApplied { port: u16 },
    /// A gated transition lost its compare-and-set and is dropped (`CasCompare`, the lost branch).
    CasConflict { port: u16 },
    /// A write to a scenario's key decided, before writing, whether it moves the state
    /// (`SetRead`, `DelRead`).
    ScenarioChangeRead { port: u16, changed: bool },
    /// A transition took its stamp from the load clock, not yet published (`Issue`).
    TransitionStampIssued { port: u16, stamp: i64 },
    /// A transition's stamp reached the stamp map (`Publish`).
    TransitionStamped { port: u16, stamp: i64 },
    /// A script run built its imposter's live state from the cell (`Start`, with none live). `key`
    /// names the cell, as the pool's routes do.
    ScriptSeeded { port: u16, key: usize },
    /// A script run's end saved the live state to the cell (`EndSave`).
    ScriptSaved { port: u16, key: usize },
    /// A script run's end gave its answer, or dropped it for a caller gone (`EndFinish`).
    ScriptFinished { port: u16, key: usize },
    /// A script run's end let go of its jobs and its claim on the worker (`EndRelease`).
    ScriptReleased { port: u16, key: usize },
}

/// What the engine calls at each [`Point`]. Implementations must be cheap and must not block:
/// some points fire on the request path.
pub trait Probe: Send + Sync + 'static {
    /// The engine took `point`.
    fn at(&self, point: Point);
}

/// The probe that records nothing, for an embedder that wants to name one explicitly. The
/// manager installs none by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoProbe;

impl Probe for NoProbe {
    fn at(&self, _point: Point) {}
}

/// A shared probe handle, `None` in production.
pub(crate) type ProbeHandle = Option<Arc<dyn Probe>>;

/// Fire `point` on `probe` when one is installed.
#[inline]
pub(crate) fn fire(probe: &ProbeHandle, point: Point) {
    if let Some(probe) = probe {
        probe.at(point);
    }
}
