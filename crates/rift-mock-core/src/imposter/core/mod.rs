//! Core Imposter struct and implementation.
//!
//! This module contains the Imposter struct which represents a single
//! running imposter instance with its configuration, stubs, and state.

/// Default scenario state when a `(flow_id, scenario)` entry is absent (WireMock parity).
pub const INITIAL_SCENARIO_STATE: &str = "Started";

use super::response::{
    create_response_preview, create_stub_from_proxy_response, execute_stub_response_with_rift,
};
use super::types::{
    DebugImposter, DebugResponsePreview, DebugStubInfo, ImposterConfig, ImposterError,
    MatchOutcome, ProxyResponse, RecordedRequest, ResponseMode, RiftResponseExtension, Stub,
    StubResponse,
};
use crate::backends::InMemoryFlowStore;
use crate::behaviors::{HasRepeatBehavior, RuleCycler};
use crate::extensions::flow_state::FlowStore;
use crate::recording::{
    ClaimOutcome, LocalProxyStore, ProxyMode, ProxyRecordingStore, RecordedResponse,
    RequestSignature,
};
use anyhow::Context;
use arc_swap::{ArcSwap, ArcSwapOption};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Request timeout for the shared proxy HTTP client.
const PROXY_HTTP_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default time-to-live for the fallback in-memory flow store when a stub declares
/// scenario state but no explicit flow-state TTL is configured.
const DEFAULT_FLOW_STATE_TTL_SECS: u64 = 300;

/// Process-wide default client for `proxy` stubs, built from the default trust policy.
///
/// Held behind a manual try-init rather than `get_or_init` (issue #974): building now consults the
/// OS trust store and can genuinely fail, and a failure must not be cached as a poisoned success —
/// the error is returned and the next call retries.
static DEFAULT_UPSTREAM_CLIENT: std::sync::OnceLock<Arc<reqwest::Client>> =
    std::sync::OnceLock::new();

/// The default client for `proxy` stubs when no operator-configured one was injected.
///
/// # Errors
/// When the outbound trust policy cannot be realised — no usable trust anchors, or a reqwest
/// builder failure. Returned rather than panicked: this replaces an `.expect` that aborted the
/// process on a TLS/DNS misconfiguration, the same fix #543 made for the reverse-proxy client.
fn default_upstream_client() -> anyhow::Result<Arc<reqwest::Client>> {
    if let Some(client) = DEFAULT_UPSTREAM_CLIENT.get() {
        return Ok(Arc::clone(client));
    }
    let client = build_upstream_client(&crate::proxy::OutboundTls::default())?;
    // A racing caller may have stored its own equivalent client first; keep whichever landed.
    Ok(Arc::clone(DEFAULT_UPSTREAM_CLIENT.get_or_init(|| client)))
}

/// The client this imposter proxies with: the injected one, else the process-wide default.
///
/// # Errors
/// Only on the default path, when the trust policy cannot be realised. Surfacing here rather than
/// at construction is deliberate — it makes the failure reachable only from a request that
/// actually proxies, where the operator can act on it.
pub(crate) fn resolve_upstream_client(
    injected: Option<&Arc<reqwest::Client>>,
) -> anyhow::Result<Arc<reqwest::Client>> {
    match injected {
        Some(client) => Ok(Arc::clone(client)),
        None => default_upstream_client(),
    }
}

/// Build a `proxy`-stub upstream client under `policy` (issue #974).
///
/// The timeout and pooling settings live here rather than at the call site so the operator-supplied
/// client and the default one cannot drift apart.
///
/// # Errors
/// When the policy cannot be realised — see [`OutboundTls::client_config`](crate::proxy::OutboundTls::client_config).
pub fn build_upstream_client(
    policy: &crate::proxy::OutboundTls,
) -> anyhow::Result<Arc<reqwest::Client>> {
    build_upstream_client_presenting(policy, None)
}

/// [`build_upstream_client`], presenting `identity` to an upstream that asks for a client
/// certificate (issue #1609). The same timeout, redirect and pooling settings, so the two clients
/// cannot drift apart.
///
/// # Errors
/// As [`build_upstream_client`].
pub(crate) fn build_upstream_client_presenting(
    policy: &crate::proxy::OutboundTls,
    identity: Option<&crate::proxy::ClientIdentity>,
) -> anyhow::Result<Arc<reqwest::Client>> {
    Ok(Arc::new(
        policy
            .reqwest_builder_with_identity(identity)?
            .timeout(PROXY_HTTP_CLIENT_TIMEOUT)
            // Relay a 30x as Mountebank does (issue #1594); following it would also record the
            // redirect target's answer under the origin's predicates.
            .redirect(reqwest::redirect::Policy::none())
            // Keep connection pooling on (issue #482): every proxied request otherwise paid a
            // fresh TCP + TLS handshake. Staleness is bounded by reqwest's default
            // `pool_idle_timeout` (90s) — the standard reqwest pooling tradeoff.
            .build()
            .context("building the proxy HTTP client")?,
    ))
}

/// Process-wide slot mint: globally unique tokens are trivially per-imposter unique, and a
/// global counter avoids threading imposter context into every construction site.
static NEXT_STUB_SLOT: AtomicU64 = AtomicU64::new(1);

/// Per-imposter source of `_rift.conditional` load stamps (issue #1301).
///
/// `Last-Modified` and its `If-Modified-Since` comparison are whole seconds, so two content changes
/// inside one second would share a stamp, and a client that revalidates with `If-Modified-Since`
/// alone (the Optimizely SDKs) would be told `304` about the second one. Stamps from this clock are
/// therefore strictly increasing at whole-second resolution: the wall clock when it has moved past
/// the last stamp, else the last stamp plus one second. The price is that rapid changes can stamp a
/// second or so ahead of the wall clock.
///
/// Take exactly one stamp per content-changing operation, and none for an operation that changes
/// nothing — a no-op must not push the clock ahead. Callers hold the imposter's `stubs_write`, or
/// own the imposter outright while constructing it — except a scenario transition (issue #1307),
/// which changes what is served without a stub write and stamps from the request path; `next` is a
/// single atomic step, so that cannot hand two callers the same second. Transitions stamp only for
/// scenarios gating a conditional stub, which keeps a busy FSM from running the clock ahead.
#[derive(Debug)]
pub(crate) struct LoadClock(AtomicI64);

impl LoadClock {
    pub(crate) fn new() -> Self {
        Self(AtomicI64::new(i64::MIN))
    }

    /// The last stamp handed out, in seconds.
    pub(crate) fn floor_secs(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Never stamp at or below `floor_secs` from now on.
    fn raise_to(&self, floor_secs: i64) {
        self.0.fetch_max(floor_secs, Ordering::Relaxed);
    }

    /// The stamp for one content-changing operation.
    pub(crate) fn next(&self) -> chrono::DateTime<chrono::Utc> {
        let now = chrono::Utc::now();
        // One atomic step, so two concurrent callers can never be handed the same second — a
        // scenario transition stamps from the request path, outside `stubs_write` (issue #1307).
        let stamp_above = |floor: i64| {
            if now.timestamp() > floor {
                now
            } else {
                // Only `None` past year 262143, where `now` is as good an answer as any.
                chrono::DateTime::from_timestamp(floor.saturating_add(1), 0).unwrap_or(now)
            }
        };
        // `update` returns the floor the winning CAS replaced; `stamp_above` is a pure function of
        // it, so recomputing yields exactly the stamp that was stored.
        let floor = self
            .0
            .update(Ordering::Relaxed, Ordering::Relaxed, |floor| {
                stamp_above(floor).timestamp()
            });
        stamp_above(floor)
    }
}

/// Whether `a` and `b` are eligible for exactly the same requests (issue #1303): the predicates and
/// the gates evaluated next to them. Responses, `id` and `route_pattern` (which only fills path
/// params) cannot change which stub answers a request, so they are not compared.
pub(crate) fn matches_alike(a: &Stub, b: &Stub) -> bool {
    a.predicates == b.predicates && a.gate() == b.gate()
}

/// Stamp the states in `range` as loaded at `at`, keeping cycler and slot (issue #1303): first
/// match wins, so when a stub ahead of these is removed, moved or narrowed, they may now answer
/// requests it answered, and a client holding its newer stamp must not be told `304`.
pub(crate) fn restamp(
    stubs: &mut [Arc<StubState>],
    range: impl std::ops::RangeBounds<usize>,
    at: chrono::DateTime<chrono::Utc>,
) {
    for state in &mut stubs[(range.start_bound().cloned(), range.end_bound().cloned())] {
        *state = Arc::new(StubState::clone(state).with_loaded_at(at));
    }
}

/// The first position at which `old` and `new` stop being eligible for the same requests, stub by
/// stub (issue #1303). Every stub from there on may answer requests differently, so it takes the
/// operation's stamp. Positional on purpose: an insert in the middle also re-stamps the stubs
/// behind it, which costs a poller one refetch but never serves a stale `304`.
pub(crate) fn first_divergence<'a>(
    old: impl IntoIterator<Item = &'a Stub>,
    new: impl IntoIterator<Item = &'a Stub>,
) -> usize {
    let mut new = new.into_iter();
    let mut k = 0;
    for a in old {
        match new.next() {
            Some(b) if matches_alike(a, b) => k += 1,
            _ => return k,
        }
    }
    k
}

/// The cycle after Mountebank's `addResponse` pushes response `new` onto the tail of a stub's
/// rotating queue (issue #1513). `cycle` is the current order (`None`: `responses` with their
/// `repeat`s) and `(r, q)` the in-range cursor, whose next answer is the queue's head.
///
/// Returns `None` when the cursor is at the start of a display-order cycle: the queue is then the
/// display order plus `new`, so no order is needed and record-then-replay never builds one. Else
/// the queue `Q` (head first), rotated right by `r` when `keep_cursor` so that `order[r] == Q[0]`
/// and the cursor already points at the head; unrotated otherwise, for a cursor restarted at 0.
fn cycle_after_append(
    cycle: Option<Vec<u32>>,
    responses: &[StubResponse],
    (r, q): (u32, u32),
    new: u32,
    keep_cursor: bool,
) -> Option<Vec<u32>> {
    let (r, q) = (r as usize, q as usize);
    let mut queue: Vec<u32> = match cycle {
        Some(order) => {
            let mut queue = order[r..].to_vec();
            queue.extend_from_slice(&order[..r]);
            queue
        }
        None if responses.is_empty() || (r, q) == (0, 0) => return None,
        None => {
            let repeat = |i: usize| responses[i].get_repeat().unwrap_or(1).max(1) as usize;
            let expanded: Vec<u32> = (0..responses.len())
                .flat_map(|i| std::iter::repeat_n(i as u32, repeat(i)))
                .collect();
            let head = (0..r).map(repeat).sum::<usize>() + q.min(repeat(r) - 1);
            let mut queue = expanded[head..].to_vec();
            queue.extend_from_slice(&expanded[..head]);
            queue
        }
    };
    queue.push(new);
    if keep_cursor {
        queue.rotate_right(r);
    }
    Some(queue)
}

#[derive(Debug, Clone)]
pub struct StubState {
    pub(crate) stub: Stub,
    cycler: Arc<RuleCycler>,
    /// Slot token for sequencer keying (issue #313): minted at insertion, preserved by
    /// in-place replaces (which keep the StubState), dropped with the slot.
    pub(crate) slot: u64,
    /// `stub`'s content hash, computed on first use and kept for the state's lifetime (issue
    /// #1254): a state never changes its `stub` (an edit builds a new state, #287), so the hash
    /// cannot go stale. Lazy so a mutation that builds states (proxy recording appends a response
    /// per request) pays nothing unless a reconcile or a sequencer asks for the key.
    content_hash: OnceLock<u64>,
    /// When this content was loaded: `_rift.conditional`'s `"lastModified": "load"` (issue #1280).
    /// Set wherever a state gets new content and never otherwise, so a reconcile that keeps an
    /// unchanged stub's state (#1265) keeps its `Last-Modified` too. The admin replace doors decide
    /// "new" by content hash (#1294), and take the stamp from the imposter's [`LoadClock`] (#1301);
    /// the constructors' wall-clock default is only what proxy recording keeps.
    loaded_at: chrono::DateTime<chrono::Utc>,
    /// The `proxyOnce` claim this stub was recorded under (issue #1386). Kept while the state is
    /// (a move, a restamp, a reconcile that keeps it) and gone with any new content, so the claim
    /// lives exactly as long as the recording; see [`Imposter::forget_orphaned_claims`].
    pub(crate) recorded_claim: Option<Arc<RequestSignature>>,
    /// The stub's cycle once a `proxyAlways` append landed mid-cycle (issue #1513): position ->
    /// index into `stub.responses`, repeat-expanded, so the cursor counts positions with every
    /// repeat 1. `None` is the display order with `repeat`. Runtime only: not in `Stub`, the
    /// content hash or any stored form, and dropped by any new content.
    cycle: Option<Arc<[u32]>>,
}

impl StubState {
    #[must_use]
    pub fn new(stub: Stub) -> Self {
        Self {
            stub,
            cycler: Arc::new(RuleCycler::new()),
            slot: NEXT_STUB_SLOT.fetch_add(1, Ordering::Relaxed),
            content_hash: OnceLock::new(),
            loaded_at: chrono::Utc::now(),
            recorded_claim: None,
            cycle: None,
        }
    }

    /// When this stub's content was loaded (see the field).
    pub(crate) fn loaded_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.loaded_at
    }

    /// This state, stamped as loaded at `at`: for a fresh slot whose content is not new.
    #[must_use]
    pub(crate) fn with_loaded_at(mut self, at: chrono::DateTime<chrono::Utc>) -> Self {
        self.loaded_at = at;
        self
    }

    /// A new slot for `stub`, whose content hash the caller already computed.
    #[must_use]
    pub(crate) fn with_hash(stub: Stub, hash: u64) -> Self {
        Self {
            content_hash: OnceLock::from(hash),
            ..Self::new(stub)
        }
    }

    /// The stub's content hash (see [`crate::imposter::reconcile::content_hash`]).
    pub(crate) fn content_hash(&self) -> u64 {
        *self
            .content_hash
            .get_or_init(|| crate::imposter::reconcile::content_hash(&self.stub))
    }

    /// The stub's sequencer key, `stub_key(stub, 0)`, from the cached hash.
    pub(crate) fn sequence_key(&self) -> String {
        crate::imposter::reconcile::key_for(self.stub.id.as_deref(), self.content_hash(), 0)
    }

    #[must_use]
    pub fn get_next_response(&self) -> Option<&StubResponse> {
        let responses = &self.stub.responses;
        if responses.is_empty() {
            return None;
        }
        if let Some(cycle) = &self.cycle {
            let pos = self
                .cycler
                .get_response_index_advance(cycle.len() as u32, |_| None);
            return self.response_at(cycle, pos);
        }

        let repeat_for_response = |idx| responses.get(idx as usize).and_then(|r| r.get_repeat());
        let response_idx = self
            .cycler
            .get_response_index_advance(responses.len() as u32, repeat_for_response);
        responses.get(response_idx as usize)
    }

    #[must_use]
    pub fn peek_response(&self) -> Option<&StubResponse> {
        if let Some(cycle) = &self.cycle {
            let pos = self.cycler.peek_response_index(cycle.len() as u32);
            return self.response_at(cycle, pos);
        }
        let responses = &self.stub.responses;
        let response_idx = self.cycler.peek_response_index(responses.len() as u32);
        self.stub.responses.get(response_idx as usize)
    }

    fn response_at(&self, cycle: &[u32], pos: u32) -> Option<&StubResponse> {
        let index = *cycle.get(pos as usize)?;
        self.stub.responses.get(index as usize)
    }

    /// The cycle a sequencer counts over: `None` for the display order with `repeat`, else the
    /// positions of the mid-cycle order (see the `cycle` field).
    pub(crate) fn cycle(&self) -> Option<&[u32]> {
        self.cycle.as_deref()
    }

    /// The local cursor, `(response index, repeat index)`, or `(position, 0)` under a cycle.
    pub(crate) fn cursor(&self) -> (u32, u32) {
        self.cycler.peek_position()
    }

    /// A copy carrying `stub` but preserving this slot's response-cycling state (the shared
    /// `cycler`) and `slot`. Used by the reconcile/replace paths to swap stub content in place
    /// now that states live behind `Arc` and cannot be mutated through a shared reference.
    ///
    /// New content drops a mid-cycle order (#1513), as Mountebank's `wrap()` resets the queue. So
    /// the cursor, a position in that order, is moved to the response it named, keeping "the
    /// cursor survives an in-place replace" in display terms.
    #[must_use]
    pub(crate) fn with_stub(&self, stub: Stub) -> Self {
        if let Some(cycle) = &self.cycle {
            let pos = self.cycler.peek_response_index(cycle.len() as u32);
            if let Some(&index) = cycle.get(pos as usize) {
                self.cycler.set(index);
            }
        }
        self.rebuilt(stub)
    }

    /// This slot (cycler and slot token) carrying `stub` as new content, in display order.
    fn rebuilt(&self, stub: Stub) -> Self {
        Self {
            stub,
            cycler: Arc::clone(&self.cycler),
            slot: self.slot,
            content_hash: OnceLock::new(),
            loaded_at: chrono::Utc::now(),
            recorded_claim: None,
            cycle: None,
        }
    }

    /// A `proxyAlways` recording appended to this stub (issue #1513). Mountebank serves a stub from
    /// a rotating queue and pushes an appended response onto its tail, so an append in the middle
    /// of a cycle is served after the rest of the current cycle, not at its list position. The
    /// display list stays append order, as Mountebank's `toJSON` shows it.
    ///
    /// `cursor` is where the stub's next answer comes from, read under the same write lock: the
    /// local cycler's [`cursor`](Self::cursor), or a sequencer's `peek` with a repeat index of 0.
    /// With `keep_cursor` (the local cycler) the order is built so that position `cursor.0` holds
    /// the queue's head, so the shared cycler is never rewritten. A request already running on the
    /// old snapshot can advance it between the read and the swap and serve one element out of
    /// Mountebank's order, the same window the display-order cycler has always had. Without it (a
    /// sequencer, whose key may change with the content) the head is position 0 and the caller
    /// restarts the sequencer's cursor; see [`Imposter::mutate_stubs`].
    #[must_use]
    pub(crate) fn with_appended(
        &self,
        extra: Vec<StubResponse>,
        cursor: (u32, u32),
        keep_cursor: bool,
    ) -> Self {
        let mut stub = self.stub.clone();
        let mut cycle: Option<Vec<u32>> = self.cycle.as_deref().map(<[u32]>::to_vec);
        let len = cycle.as_ref().map_or(stub.responses.len(), Vec::len) as u32;
        // A cursor past the end (the stub shrank under it) serves the last response, as the
        // cycler clamps it; the order must start there too, and the cycler must say so.
        let in_range = if len == 0 {
            (0, 0)
        } else if cursor.0 >= len {
            (len - 1, 0)
        } else {
            cursor
        };
        if in_range != cursor && keep_cursor {
            self.cycler.set(in_range.0);
        }
        let mut cursor = in_range;
        for response in extra {
            let new = stub.responses.len() as u32;
            cycle = cycle_after_append(cycle, &stub.responses, cursor, new, keep_cursor);
            stub.responses.push(response);
            if !keep_cursor && cycle.is_some() {
                cursor = (0, 0);
            }
        }
        Self {
            cycle: cycle.map(Into::into),
            ..self.rebuilt(stub)
        }
    }

    /// This stub with a fresh response cycle (#1515): the same content, content hash, load stamp
    /// and recorded claim, but a new cycler and a new slot, so neither the embedded cursor nor a
    /// sequencer keyed by slot carries over. In-flight requests holding the old state advance the
    /// old cycler, never the new one.
    #[must_use]
    pub(crate) fn with_fresh_cycle(&self) -> Self {
        Self {
            stub: self.stub.clone(),
            cycler: Arc::new(RuleCycler::new()),
            slot: NEXT_STUB_SLOT.fetch_add(1, Ordering::Relaxed),
            content_hash: self.content_hash.clone(),
            loaded_at: self.loaded_at,
            recorded_claim: self.recorded_claim.clone(),
            cycle: None,
        }
    }

    /// [`with_stub`](Self::with_stub) for a stub whose content hash the caller already computed.
    #[must_use]
    pub(crate) fn with_stub_hashed(&self, stub: Stub, hash: u64) -> Self {
        Self {
            content_hash: OnceLock::from(hash),
            ..self.with_stub(stub)
        }
    }
}

/// Runtime state of an imposter
pub struct Imposter {
    pub config: ImposterConfig,
    /// The stubs, the Stage-1 candidate index over those exact stubs, and the snapshot-wide
    /// matching gates — one unit (issue #707).
    ///
    /// Each `StubState` sits behind `Arc` so a matched request takes a refcount bump instead of
    /// deep-cloning it (issue #287), and the whole snapshot behind `ArcSwap` so the match hot path
    /// reads wait-free (one `load()`, no lock) while a mutation atomically swaps in a fresh
    /// snapshot (issue #291). Bundling the stubs with their index (previously two `ArcSwap`s kept
    /// in sync by convention) makes a torn (stubs N, index N+1) read unrepresentable and halves the
    /// hot path's atomic loads. Serialize *writers* through [`Self::mutate_stubs`] /
    /// `stubs_write`; never `store()` this field directly.
    stubs_snapshot: ArcSwap<StubSnapshot>,
    /// Serializes stub *writers* so a read-copy-update (clone snapshot → mutate → store) can't
    /// lose a concurrent update (issue #291). Off the request hot path — held only by admin /
    /// reload / proxy-record mutations, never by readers.
    stubs_write: Mutex<()>,
    /// Where new content's `Last-Modified: load` comes from (issue #1301).
    pub(crate) load_clock: LoadClock,
    /// When each scenario last changed state, by scenario name (issue #1307). A transition changes
    /// which stub answers with no stub changing, so the served `Last-Modified` folds in the
    /// transitions of every scenario gating the answering stub or a stub ahead of it. Keyed by name,
    /// not by flow: config-bounded, where flows are unbounded.
    /// Copied on write: read once per request before matching, without a lock.
    scenario_transitions: ArcSwap<HashMap<String, chrono::DateTime<chrono::Utc>>>,
    /// Proxy-recording backend (issue #315); defaults to a private port-scoped
    /// [`LocalProxyStore`] for this imposter's mode, or the embedder's shared store injected
    /// via [`ImposterManager::with_proxy_store`](crate::imposter::ImposterManager::with_proxy_store).
    pub(crate) proxy_store: Arc<dyn ProxyRecordingStore>,
    /// Client used for every `proxy` stub's upstream call (issue #974). `None` means "resolve the
    /// process-wide default lazily, on first use"; [`ImposterManager::with_upstream_client`] fills
    /// it with the operator-configured one, exactly as `proxy_store` is injected.
    ///
    /// Deliberately NOT eagerly resolved at construction. Building the default policy reads the OS
    /// trust store and can fail (a distroless image with no CA bundle), and an imposter with no
    /// `proxy` stub at all must not fail to be created over an outbound policy it never uses —
    /// still less when the operator supplied a working `--upstream-ca-file` that the manager is
    /// about to inject over the top of it.
    pub(crate) upstream_client: Option<Arc<reqwest::Client>>,
    /// Clients for `proxy` stubs that present a client certificate (issue #1609), one per distinct
    /// identity, with the trust policy they are built under. The manager shares one across its
    /// imposters; a standalone imposter has its own, on the default policy.
    pub(crate) identity_clients: Arc<IdentityClients>,
    /// The body limits and in-flight budget this imposter's requests and proxied responses draw on
    /// (issue #1439). The manager injects its shared guard; a standalone imposter has its own.
    pub(crate) body_guard: Arc<super::body_limits::BodyGuard>,
    /// Admin SSE event bus (issue #461), shared from the manager so recorded requests fan out to
    /// streaming clients. `None` for a standalone imposter (no manager) — then nothing is published.
    pub(crate) event_bus: Option<Arc<super::events::AdminEventBus>>,
    /// Last-chance no-match hook (issue #819), shared from the manager. `None` = no interceptor;
    /// the request falls through to defaultForward/defaultResponse/empty-200 as before.
    pub(crate) no_match_interceptor:
        Option<Arc<dyn crate::extensions::no_match::NoMatchInterceptor>>,
    /// Synchronous exchange inspector (issue #966), shared from the manager. `None` = no
    /// inspector; every request is matched and every response is written exactly as before —
    /// nothing else about the request or response path changes.
    pub(crate) exchange_inspector:
        Option<Arc<dyn crate::extensions::exchange_inspector::ExchangeInspector>>,
    /// Recorded-request storage (issue #314); defaults to a private LocalJournal,
    /// or the embedder's shared journal injected via the manager.
    pub(crate) journal: Arc<dyn crate::imposter::journal::RequestJournal>,
    /// `{port}.requests.jsonl` (issue #1608): set by the manager when `journal` is the imposter's
    /// own [`LocalJournal`](crate::imposter::LocalJournal), it records requests, and the manager
    /// has a datadir. Written only while [`persistence`](Self::persistence) is `Datadir`.
    pub(crate) journal_file: Option<Arc<JournalFile>>,
    /// Whether imposter is enabled
    pub enabled: AtomicBool,
    /// Whether a response cycle may have moved since the manager last wrote `{port}.cursors` (issue
    /// #1603): set by every served response and every stub edit, taken by the flusher.
    cursors_dirty: AtomicBool,
    /// `true` = [`Persistence::Datadir`](crate::imposter::Persistence) (issue #1122). Atomic
    /// because an apply can move a running, `Arc`-shared imposter from one store to the other.
    persist_to_datadir: AtomicBool,
    /// Creation timestamp (for future metrics/admin display)
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Shutdown signal sender (for future graceful shutdown)
    pub shutdown_tx: Option<broadcast::Sender<()>>,
    /// Accept-loop task handles (issue #596). The manager stores them here after spawning the
    /// loop(s) so `delete` can `await` them — guaranteeing every listener socket is dropped
    /// (bind released) before delete returns, instead of the old fire-and-forget teardown.
    /// One handle in the default topology; N under per-core fan-out (RFC-712, issue #745), all
    /// stopped by the same shutdown broadcast and drained through the one shared
    /// `conn_tracker`. `Mutex` because they are set after the imposter is already `Arc`-wrapped,
    /// and taken (to be awaited) on teardown.
    pub(crate) serve_handles: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Tracks this generation's live per-connection tasks (issue #596) so `delete` can drain them
    /// (each already reacts to `shutdown_tx` with a hyper graceful shutdown, #207) within a bound.
    pub(crate) conn_tracker: tokio_util::task::TaskTracker,
    /// Cancelled when this generation is deleted or replaced (issue #1643). Every request still
    /// being answered races it in [`handle_imposter_request`](super::handler::handle_imposter_request)
    /// and is cut off with no response, as Mountebank destroys every socket of a deleted imposter.
    /// Level-triggered, unlike `shutdown_tx`, so a request that arrives just before the delete
    /// still sees it. Never cancelled for an imposter no manager started.
    pub(crate) cancel: tokio_util::sync::CancellationToken,
    /// Flow store for Rift extensions (stateful scripting)
    pub flow_store: Arc<dyn FlowStore>,
    /// Whether `flow_store` is the built-in in-memory store rather than a
    /// [`FlowStoreProvider`](crate::extensions::flow_state::FlowStoreProvider)'s. Only the built-in
    /// one is this imposter's to clear on a runtime reset (issue #1518): a recreate would get the
    /// provider's store back from the provider.
    flow_store_is_default: bool,
    /// Pluggable response-cursor backend (issue #313); None = embedded per-stub cycler.
    pub(crate) sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
    /// Cached stub-overlap analysis warnings (issue #423). `None` = dirty; recomputed lazily on the
    /// next [`Self::stub_warnings`] read and reused until the next `mutate_stubs`. Keeps the O(n)
    /// analysis off the per-`GET` hot path and gives HTTP and embedded/FFI one shared code path.
    stub_warnings: ArcSwapOption<Vec<crate::extensions::stub_analysis::StubWarning>>,
    /// [`imposter_level_view`] of `config`, computed on the first reconcile that asks (issue
    /// #1254). `config` never changes after construction, so neither does this.
    imposter_level: OnceLock<Result<serde_json::Value, String>>,
    /// What this imposter's scripts may `require()` (issue #1611): chosen by the door that last
    /// wrote its stubs, never part of `config`. [`ScriptFileAccess::Refused`] until a door says
    /// otherwise.
    script_files: ArcSwap<crate::imposter::ScriptFileAccess>,
}

/// `config` as `apply_config` compares it at the imposter level: every field except `stubs`, with
/// `enabled` normalized (an enabled-only change toggles in place, #817). The stubs are moved out for
/// the serialization and put back, rather than cloned (issue #1254).
pub(crate) fn imposter_level_view(
    config: &mut ImposterConfig,
) -> serde_json::Result<serde_json::Value> {
    let stubs = std::mem::take(&mut config.stubs);
    let enabled = std::mem::replace(&mut config.enabled, true);
    // Compared as the imposter will hold it: without a non-https `ciphers` (issue #1576).
    let ciphers = config.take_non_https_ciphers();
    let view = serde_json::to_value(&*config);
    config.stubs = stubs;
    config.enabled = enabled;
    if ciphers.is_some() {
        config.ciphers = ciphers;
    }
    view
}

impl Imposter {
    /// Create a new imposter from config (no custom flow-store provider).
    pub fn new(config: ImposterConfig) -> anyhow::Result<Self> {
        Self::new_with_provider(config, None)
    }

    /// Create a new imposter, consulting `provider` for its flow store before the built-in
    /// `_rift.flowState` selection (issue #312).
    pub fn new_with_provider(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_hooks(config, provider, None)
    }

    /// Create a new imposter with all embedder hooks: the flow-store `provider` (#312) and
    /// a pluggable response `sequencer` (#313; None = embedded per-stub cycler).
    pub fn new_with_hooks(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_hooks_and_journal(config, provider, sequencer, None)
    }

    /// Create a new imposter with all embedder hooks, including the request journal (#314).
    ///
    /// `config.port` must be resolved (Some) before the imposter serves requests: a shared
    /// journal keys by port, and unresolved ports would silently multiplex onto slot 0.
    ///
    /// Fails (issue #530) when `_rift.flowState.ttlSeconds` is not positive. The implicit NoOp
    /// (no `_rift.flowState` configured) still succeeds.
    pub fn new_with_hooks_and_journal(
        config: ImposterConfig,
        provider: Option<&Arc<dyn crate::extensions::flow_state::FlowStoreProvider>>,
        sequencer: Option<Arc<dyn crate::behaviors::ResponseSequencer>>,
        journal: Option<Arc<dyn crate::imposter::journal::RequestJournal>>,
    ) -> anyhow::Result<Self> {
        let mut config = config;
        // Dropped silently on a non-https imposter, as Mountebank's http server drops it (#1576):
        // it is not printed back, so the readback shows it was not kept.
        if let Some(dropped) = config.take_non_https_ciphers() {
            tracing::debug!(port = ?config.port, ciphers = %dropped, "`ciphers` dropped: not https");
        }
        let load_clock = LoadClock::new();
        let loaded_at = load_clock.next();
        let stubs: Vec<Arc<StubState>> = config
            .stubs
            .iter()
            .map(|stub| Arc::new(StubState::new(stub.clone()).with_loaded_at(loaded_at)))
            .collect();
        // Extract proxy mode from stubs (use first proxy response's mode)
        // Keys parsed but not acted on (issues #999, #1152). They are KEPT rather than deleted:
        // `RiftConfig` has no `deny_unknown_fields`, so removing one would turn a known-ignored key
        // into an unknown-ignored one, which is strictly worse for the operator. Logged here for
        // the doors that never see an API response (`--configfile`, `--datadir`, the C-ABI); the
        // admin API also returns the same list in `_rift.warnings`.
        for ignored in crate::extensions::stub_analysis::ignored_config_keys(&config, &config.stubs)
        {
            tracing::warn!(port = ?config.port, "{}", ignored.message);
        }

        Self::report_unknown_proxy_modes(&config.stubs);

        // Initialize flow store: a registered provider wins; otherwise the built-in
        // `_rift.flowState` selection.
        //
        // Metered here rather than inside `create_flow_store` (issue #999): this is the single
        // point every selection path funnels through — provider, `inmemory` and the NoOp
        // fallback — so one wrap
        // counts `rift_flow_state_ops_total` for all of them. The decorator forwards every trait
        // method, including `is_blocking`, so backend behaviour is unchanged.
        let provided = provider.and_then(|provider| provider.provide(&config));
        let flow_store_is_default = provided.is_none();
        let flow_store: Arc<dyn FlowStore> =
            Arc::new(crate::extensions::flow_state::MeteredFlowStore::new(
                Self::create_flow_store(&config, provided)?,
            ));

        let enabled = config.enabled;
        Ok(Self {
            config,
            stubs_snapshot: ArcSwap::from_pointee(StubSnapshot::build(stubs)),
            stubs_write: Mutex::new(()),
            load_clock,
            scenario_transitions: ArcSwap::from_pointee(HashMap::new()),
            // Each request claims with its own proxy response's mode (issue #1387).
            proxy_store: Arc::new(LocalProxyStore::new(ProxyMode::ProxyOnce)),
            upstream_client: None,
            identity_clients: Arc::default(),
            body_guard: Arc::default(),
            event_bus: None,
            no_match_interceptor: None,
            exchange_inspector: None,
            journal: journal
                .unwrap_or_else(|| Arc::new(crate::imposter::journal::LocalJournal::default())),
            journal_file: None,
            enabled: AtomicBool::new(enabled),
            cursors_dirty: AtomicBool::new(false),
            persist_to_datadir: AtomicBool::new(true),
            created_at: chrono::Utc::now(),
            shutdown_tx: None,
            serve_handles: Mutex::new(Vec::new()),
            conn_tracker: tokio_util::task::TaskTracker::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            flow_store,
            flow_store_is_default,
            sequencer,
            stub_warnings: ArcSwapOption::empty(),
            imposter_level: OnceLock::new(),
            script_files: ArcSwap::from_pointee(crate::imposter::ScriptFileAccess::Refused),
        })
    }

    /// What this imposter's scripts may `require()` (issue #1611), as the door that last wrote its
    /// stubs chose it.
    #[must_use]
    pub fn script_file_access(&self) -> Arc<crate::imposter::ScriptFileAccess> {
        self.script_files.load_full()
    }

    /// Set what this imposter's scripts may `require()` (issue #1611), in the stub writers'
    /// critical section. A request already running keeps the access it started with. Not for a
    /// door about to write stubs: a set and a separate write race a concurrent reload (issue
    /// #1645); such a door writes through the `_from` stub writes, which narrow the access in the
    /// same critical section as the write.
    pub(crate) fn set_script_file_access(&self, access: crate::imposter::ScriptFileAccess) {
        let _writer = self.stubs_write.lock();
        self.script_files.store(Arc::new(access));
    }

    /// Which store this imposter belongs to (issue #1122).
    #[must_use]
    pub fn persistence(&self) -> crate::imposter::Persistence {
        if self.persist_to_datadir.load(Ordering::SeqCst) {
            crate::imposter::Persistence::Datadir
        } else {
            crate::imposter::Persistence::Ephemeral
        }
    }

    pub(crate) fn set_persistence(&self, persistence: crate::imposter::Persistence) {
        self.persist_to_datadir.store(
            persistence == crate::imposter::Persistence::Datadir,
            Ordering::SeqCst,
        );
    }

    /// The current stub snapshot: stubs, the index over them, and the matching gates, from a single
    /// wait-free load (issue #707). The match hot path calls this exactly once per request.
    pub(crate) fn snapshot(&self) -> arc_swap::Guard<Arc<StubSnapshot>> {
        self.stubs_snapshot.load()
    }

    /// This imposter's [`imposter_level_view`], cached. Computing it clones `config` once; a
    /// serialization failure is kept as its message.
    pub(crate) fn imposter_level_view(&self) -> &Result<serde_json::Value, String> {
        self.imposter_level.get_or_init(|| {
            let mut config = self.config.clone();
            imposter_level_view(&mut config).map_err(|e| e.to_string())
        })
    }

    /// Whether this imposter owns a listener on its port.
    ///
    /// Normally always true. It is false only for an imposter registered under
    /// [`ImposterManager::with_serve_unbound`] (issue #143) after its port bind failed: it has no
    /// accept loop, but it is in the port map and therefore still answers every in-process route
    /// (the gateway and, in the cluster build, the front door) — both resolve through
    /// `get_imposter`, and request handling reads none of the accept-loop machinery.
    ///
    /// Derived from [`Self::serve_handles`] rather than tracked in a separate flag: the manager
    /// fills the handles before claiming the port, and `delete` takes them on teardown, so for an
    /// imposter in the map "has handles" *is* "is bound". A second field could disagree with the
    /// listeners that actually exist; this cannot.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        !self.serve_handles.lock().is_empty()
    }

    /// Read-copy-update the stub vector (issue #291). Reads stay wait-free (`self.snapshot()`);
    /// a mutation takes the `stubs_write` mutex (serializing writers so no update is lost), clones
    /// the current snapshot, applies `f`, then atomically swaps the new snapshot in. `f`'s return
    /// value is passed back to the caller. Mutations are off the request hot path.
    ///
    /// The new snapshot is stored unconditionally — even when `f` reports a no-op via its return
    /// value (e.g. an out-of-bounds `Err` or a duplicate-id `false`). That is safe only because
    /// every such `f` validates *before* touching the vector, so a rejected call stores a
    /// content-identical clone. Any `f` added here must uphold that: never partially mutate the
    /// vector and then return an error/no-op, or the partial change would be committed silently.
    pub(crate) fn mutate_stubs<R>(&self, f: impl FnOnce(&mut Vec<Arc<StubState>>) -> R) -> R {
        self.mutate_stubs_from(None, f)
    }

    /// [`Self::mutate_stubs`] for a door that writes scripts (issue #1645): `door`, when given,
    /// narrows the imposter's script access to what both it and the current access allow
    /// ([`ScriptFileAccess::meet`](crate::imposter::ScriptFileAccess::meet)), in the same critical
    /// section as the write and stored before the new stubs are. A request loads the stub snapshot
    /// before the access, so it can never pair a stub this door wrote with an access wider than the
    /// door's, whatever write or reload runs alongside. In place the access only ever narrows; it
    /// widens only when an imposter is created with every stub new.
    pub(crate) fn mutate_stubs_from<R>(
        &self,
        door: Option<&crate::imposter::ScriptFileAccess>,
        f: impl FnOnce(&mut Vec<Arc<StubState>>) -> R,
    ) -> R {
        let (result, before, after) = {
            let _writer = self.stubs_write.lock();
            if let Some(door) = door {
                let narrowed = self.script_files.load().meet(door);
                self.script_files.store(Arc::new(narrowed));
            }
            let before = self.stubs_snapshot.load_full();
            let mut next: Vec<Arc<StubState>> = before.stubs().to_vec();
            let result = f(&mut next);
            // One store of stubs+index together (issue #707): the index is built from the very
            // vector it is stored with, so the two cannot diverge by construction.
            let after = Arc::new(StubSnapshot::build(next));
            self.stubs_snapshot.store(Arc::clone(&after));
            // Invalidate the cached stub-analysis warnings (issue #423). This is O(1) — the actual
            // O(n) recompute is deferred to the next `stub_warnings()` read — so high-frequency
            // mutations (e.g. proxy recording) don't pay analysis cost on every recorded stub.
            self.stub_warnings.store(None);
            // An edit can restart, move or drop a cycle; the next cursor flush writes what it left.
            self.mark_cursors_dirty();
            (result, before, after)
        };
        self.forget_orphaned_claims(before.stubs(), after.stubs());
        self.restart_reordered_sequences(before.stubs(), after.stubs());
        result
    }

    /// Restart a sequencer's cursor for each slot whose cycle order an edit built, changed or
    /// dropped (issue #1513). The sequencer counts positions in that order, and a slot's key moves
    /// with its content, so its old cursor means nothing under the new order: a `proxyAlways`
    /// append builds the order with the queue's head at 0, and new content restarts at the first
    /// response, as Mountebank's `wrap()` does. Without a sequencer the embedded cycler is kept in
    /// step by [`StubState::with_appended`] and [`StubState::with_stub`] instead. Like the claims,
    /// after the swap: a request in between can be served one answer from the old cursor.
    fn restart_reordered_sequences(&self, before: &[Arc<StubState>], after: &[Arc<StubState>]) {
        let Some(sequencer) = &self.sequencer else {
            return;
        };
        if before.iter().chain(after).all(|s| s.cycle.is_none()) {
            return;
        }
        // The order is shared by `Arc` across clones (a restamp, a move), so only a new order or
        // none-for-some counts as a change.
        let same = |a: &Option<Arc<[u32]>>, b: &Option<Arc<[u32]>>| match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        let port = self.config.port.unwrap_or(0);
        for new in after {
            if let Some(old) = before.iter().find(|o| o.slot == new.slot)
                && !same(&old.cycle, &new.cycle)
            {
                sequencer.reset_scope(port, Some(&old.sequence_key()));
            }
        }
    }

    /// Forget the `proxyOnce` claims of the recordings an edit removed (issue #1386): those held
    /// by a state in `before` and by none in `after`. Called after the swap and outside
    /// `stubs_write`, since a store may be slow. A store error is logged, not propagated: the stub
    /// edit has already happened, and the cost is that identity replaying until the store is
    /// cleared, today's behaviour.
    pub(crate) fn forget_orphaned_claims(
        &self,
        before: &[Arc<StubState>],
        after: &[Arc<StubState>],
    ) {
        if before.iter().all(|s| s.recorded_claim.is_none()) {
            return;
        }
        let kept: std::collections::HashSet<&RequestSignature> = after
            .iter()
            .filter_map(|s| s.recorded_claim.as_deref())
            .collect();
        let port = self.journal_port();
        for sig in before
            .iter()
            .filter_map(|s| s.recorded_claim.as_deref())
            .filter(|sig| !kept.contains(sig))
        {
            if let Err(e) = self.proxy_store.forget_recorded(port, sig) {
                error!(port, path = %sig.path, "Failed to forget a deleted proxy recording: {e}");
            }
        }
    }

    /// The imposter's stub-overlap analysis warnings (issue #423), computed once and cached until
    /// the next stub mutation. The HTTP `GET /imposters/:port` handler and embedded/FFI consumers
    /// both read this, so analysis runs off the per-read hot path and is identical for standalone
    /// and embedded instances (which previously got no analysis at all).
    pub fn stub_warnings(&self) -> Arc<Vec<crate::extensions::stub_analysis::StubWarning>> {
        // Fast path: a warm cache is a wait-free load, so the request/read hot path never blocks.
        if let Some(cached) = self.stub_warnings.load_full() {
            return cached;
        }
        // Miss: compute-and-store under the same `stubs_write` lock `mutate_stubs` holds, so a
        // mutation's invalidation can never be lost by a slow reader that computed over a stale
        // snapshot (a store racing an invalidation would pin stale warnings). The lock is taken
        // only on a miss — once per mutation — and the O(n) analysis is off the request hot path.
        let _writer = self.stubs_write.lock();
        // Re-check under the lock: another reader may have populated it while we waited.
        if let Some(cached) = self.stub_warnings.load_full() {
            return cached;
        }
        let stubs = self.get_stubs();
        // Ignored keys first: there are few of them, and the analysis below caps its own share.
        let mut warnings =
            crate::extensions::stub_analysis::ignored_config_keys(&self.config, &stubs);
        warnings.extend(crate::extensions::stub_analysis::analyze_stubs(&stubs).warnings);
        let arc = Arc::new(warnings);
        self.stub_warnings.store(Some(Arc::clone(&arc)));
        arc
    }

    /// Continue the load clock of the imposter this one replaces on the same port (issue #1301): a
    /// client knows the URL, not the imposter, so a re-created imposter must not stamp its stubs at
    /// or before what the old one served. Restamps this imposter's stubs when they would.
    pub(crate) fn continue_load_clock(&self, floor_secs: i64) {
        self.mutate_stubs(|stubs| {
            self.load_clock.raise_to(floor_secs);
            if stubs.iter().all(|s| s.loaded_at.timestamp() > floor_secs) {
                return;
            }
            restamp(stubs, .., self.load_clock.next());
        });
    }

    /// Replace all stubs. Every slot is new, so every response cycle restarts. Stamps are aligned
    /// by position: up to the first position where the old and new stubs stop matching alike, a
    /// stub with unchanged content keeps its `Last-Modified: load` (issue #1294); every other stub
    /// shares one stamp from the load clock (issues #1301, #1303) — a stub that moved keeps nothing,
    /// since what it answers may have changed.
    pub fn replace_stubs(&self, new_stubs: Vec<Stub>) {
        self.replace_stubs_from(None, new_stubs);
    }

    /// [`Self::replace_stubs`] for a door that writes scripts: see [`Self::mutate_stubs_from`]
    /// (issue #1645).
    pub(crate) fn replace_stubs_from(
        &self,
        door: Option<&crate::imposter::ScriptFileAccess>,
        new_stubs: Vec<Stub>,
    ) {
        self.mutate_stubs_from(door, |stubs| self.overwrite_all(stubs, new_stubs));
    }

    /// [`Self::replace_stubs`]' body, for a caller already inside [`Self::mutate_stubs`]: every
    /// slot is new, and stamps are aligned by position.
    pub(crate) fn overwrite_all(&self, stubs: &mut Vec<Arc<StubState>>, new_stubs: Vec<Stub>) {
        let k = first_divergence(stubs.iter().map(|s| &s.stub), new_stubs.iter());
        let old: Vec<Arc<StubState>> = std::mem::take(stubs);
        // Taken on the first stub that needs it only: an identical replace must not push the clock.
        let mut fresh: Option<chrono::DateTime<chrono::Utc>> = None;
        stubs.extend(new_stubs.into_iter().enumerate().map(|(p, s)| {
            let hash = crate::imposter::reconcile::content_hash(&s);
            let at = match old.get(p) {
                Some(prev) if p < k && prev.content_hash() == hash => prev.loaded_at(),
                _ => *fresh.get_or_insert_with(|| self.load_clock.next()),
            };
            Arc::new(StubState::with_hash(s, hash).with_loaded_at(at))
        }));
    }

    /// Create the flow store: the store a
    /// [`FlowStoreProvider`](crate::extensions::flow_state::FlowStoreProvider) supplied
    /// (`provided`), if any, otherwise an in-memory store sized by `_rift.flowState` (issue #1463
    /// removed the named `backend` selector; a leftover `backend` key decodes into `extra` and
    /// selects nothing).
    /// Every other imposter gets a default in-memory store too (issue #1504): the admin
    /// flow-state and scenario routes and the FFI write the store outside request matching, and
    /// stubs can gain scenarios or scripts later via an in-place `PUT/POST /imposters/:port/stubs`,
    /// so no imposter can be known to never touch it. The scenario, script and `stateOps`
    /// branches remain only to log what an operator relying on the default should know.
    fn create_flow_store(
        config: &ImposterConfig,
        provided: Option<Arc<dyn FlowStore>>,
    ) -> anyhow::Result<Arc<dyn FlowStore>> {
        if let Some(store) = provided {
            // A provider store wins over any built-in selection, including an explicit
            // `_rift.flowState` — log it so an operator whose config was overridden can see why.
            if config
                .rift
                .as_ref()
                .and_then(|r| r.flow_state.as_ref())
                .is_some()
            {
                debug!(
                    "FlowStoreProvider supplied a store, overriding the imposter's _rift.flowState"
                );
            } else {
                debug!("FlowStoreProvider supplied the imposter flow store");
            }
            return Ok(store);
        }
        if let Some(flow_state_config) = config.rift.as_ref().and_then(|r| r.flow_state.as_ref()) {
            // Fail-loud config validation (issue #530): a non-positive default TTL misbehaves late
            // and inconsistently (in-memory → instant expiry of every write; a networked store → error on
            // the first write), so reject it at construction. This surfaces as a 400 via
            // ImposterError::FlowStoreConfig.
            crate::extensions::flow_state::validate_ttl_seconds(flow_state_config.ttl_seconds)?;
            info!(
                "Creating InMemory FlowStore for imposter (ttl={}s)",
                flow_state_config.ttl_seconds
            );
            return Ok(Arc::new(InMemoryFlowStore::new(
                flow_state_config.ttl_seconds as u64,
            )));
        }

        if Self::uses_scenario_fsm(&config.stubs) {
            info!("Stubs declare scenario state; using default in-memory FlowStore");
            return Ok(Arc::new(InMemoryFlowStore::new(
                DEFAULT_FLOW_STATE_TTL_SECS,
            )));
        }

        if Self::uses_script(&config.stubs) {
            // A script may call `ctx.state`/`flow_store` at runtime — not visible statically — so
            // provision a real store rather than let it silently discard writes (issue #358). This
            // is a convenience default, not a production recommendation: warn so an operator who
            // didn't mean to rely on it notices in their logs.
            tracing::warn!(
                target: "rift::script",
                ttl_seconds = DEFAULT_FLOW_STATE_TTL_SECS,
                "imposter has a _rift.script stub but no _rift.flowState configured; \
                 auto-provisioning an in-memory FlowStore. State will NOT persist across restarts \
                 or be shared across a cluster — configure _rift.flowState for production."
            );
            return Ok(Arc::new(InMemoryFlowStore::new(
                DEFAULT_FLOW_STATE_TTL_SECS,
            )));
        }

        if Self::uses_state_ops(&config.stubs) {
            // Same reasoning as the script branch (issue #969): the ops write the store on every
            // matched request, and the no-op store would discard every one of those writes
            // without a word — the most natural first config a user writes must not do that.
            tracing::warn!(
                target: "rift::state_ops",
                ttl_seconds = DEFAULT_FLOW_STATE_TTL_SECS,
                "imposter has a _rift.stateOps stub but no _rift.flowState configured; \
                 auto-provisioning an in-memory FlowStore. State will NOT persist across restarts \
                 or be shared across a cluster — configure _rift.flowState for production."
            );
            return Ok(Arc::new(InMemoryFlowStore::new(
                DEFAULT_FLOW_STATE_TTL_SECS,
            )));
        }

        Ok(Arc::new(InMemoryFlowStore::new(
            DEFAULT_FLOW_STATE_TTL_SECS,
        )))
    }

    /// Whether any `is` response carries `_rift.stateOps` (issue #969) — such a stub writes the
    /// flow store on every matched request, so its imposter needs a real store even without
    /// `_rift.flowState` configured. `is` only, which includes a script-less bare `_rift` response
    /// served as `is: {}` (issue #1685): the ops do not run on any other response shape.
    fn uses_state_ops(stubs: &[Stub]) -> bool {
        stubs
            .iter()
            .flat_map(|s| &s.responses)
            .any(|resp| match resp {
                StubResponse::Is { rift: Some(r), .. } => !r.state_ops.is_empty(),
                StubResponse::RiftScript { rift, .. } => {
                    rift.script.is_none() && !rift.state_ops.is_empty()
                }
                _ => false,
            })
    }

    /// Whether any stub references a scenario — via the declarative FSM fields
    /// (`requiredScenarioState`/`newScenarioState`) or by merely naming one (`scenarioName`, #514).
    fn uses_scenario_fsm(stubs: &[Stub]) -> bool {
        // A stub that merely names a scenario (`scenarioName`, no gate/transition) still needs a
        // real flow store (issue #514): the admin/FFI scenario surface reads and writes the store
        // directly, outside request matching, so landing on the NoOp store makes set-state/reset a
        // silent no-op. Triggering on `scenario_name` too is a strict superset of the FSM fields.
        stubs.iter().any(|s| {
            s.scenario_name.is_some()
                || s.required_scenario_state.is_some()
                || s.new_scenario_state.is_some()
        })
    }

    /// Whether any stub can execute a Rift script (`_rift.script` on an `Is` response, or a
    /// scripted `RiftScript` response) — such a stub might call `ctx.state`/`flow_store` at
    /// runtime, so its imposter needs a real flow store even without `_rift.flowState`
    /// configured (issue #358). A `RiftScript` without a script runs none: it is served as
    /// `is: {}` (issue #1685).
    fn uses_script(stubs: &[Stub]) -> bool {
        stubs
            .iter()
            .flat_map(|s| &s.responses)
            .any(|resp| match resp {
                StubResponse::Is { rift: Some(r), .. }
                | StubResponse::RiftScript { rift: r, .. } => r.script.is_some(),
                _ => false,
            })
    }

    /// Whether any current stub can trigger a connection-level TCP fault (`_rift.fault.tcp`, or a
    /// Mountebank `fault` that maps to one). Such imposters must be served HTTP/1-only: a TCP fault
    /// aborts the whole socket, which is incompatible with HTTP/2 stream multiplexing (issue #295).
    ///
    /// A `_rift.script` (RiftScript) stub also counts (issue #357 B2): a script can dynamically
    /// call `reset()` — the v2 connection-reset result constructor — which lowers to the same
    /// transport-level abort as `_rift.fault.tcp`, so a script-bearing stub must likewise be
    /// served HTTP/1-only even though its reset path isn't visible in the static config.
    pub(crate) fn uses_tcp_faults(&self) -> bool {
        use crate::imposter::fault_io::TcpFaultKind;
        let rift_has_tcp = |rift: &RiftResponseExtension| {
            rift.fault
                .as_ref()
                .and_then(|f| f.tcp.as_ref())
                .is_some_and(|t| TcpFaultKind::parse(t.kind()).is_some())
        };
        self.snapshot()
            .stubs()
            .iter()
            .flat_map(|s| &s.stub.responses)
            .any(|resp| match resp {
                StubResponse::Fault { fault, .. } => TcpFaultKind::parse(fault).is_some(),
                StubResponse::Is { rift: Some(r), .. } => rift_has_tcp(r),
                // A script may call `reset()` at runtime, so conservatively force H1-only.
                StubResponse::RiftScript { .. } => true,
                _ => false,
            })
    }

    /// The key under which this imposter's Mountebank inject/decorate script state is stored in
    /// the process-global `IMPOSTER_STATE` map — the imposter's bound listener port.
    ///
    /// `config.port` is `Some(bound_port)` for the entire life of a live imposter: the manager
    /// assigns and records the real, distinct bound port in `create_imposter_staged` *before* the
    /// imposter is constructed or serves a request, so distinct imposters (including auto-bind
    /// ones) always get distinct keys and never clobber each other's script state (issue #439).
    /// The `unwrap_or(0)` fallback is therefore unreachable for a live imposter — `0` is the
    /// documented "no live imposter" sentinel (see `rift-ffi`); it exists only to keep this
    /// total. Centralised here so the invariant has one greppable home rather than a scattered
    /// magic `config.port.unwrap_or(0)` at every script call site.
    pub(crate) fn script_state_key(&self) -> u16 {
        self.bound_port()
    }

    /// This imposter's bound listener port, carrying the same invariant [`Self::script_state_key`]
    /// documents: `Some(bound_port)` for the whole life of a live imposter, with `0` as the
    /// documented "no live imposter" sentinel, unreachable in practice.
    ///
    /// Used as the `rule_id` metric label (issue #999): the imposter path has no named rules the
    /// way the removed reverse proxy did, so the port is what identifies which imposter injected a
    /// fault. Other sites still inline `config.port.unwrap_or(0)`; this is the home for the two
    /// that need it by name, not yet a repo-wide consolidation.
    pub(crate) fn bound_port(&self) -> u16 {
        self.config.port.unwrap_or(0)
    }

    /// Report each proxy response whose `mode` this engine does not know. A config door refuses
    /// one (issue #1314), so it was replayed from an older engine's store; it runs as Mountebank's
    /// default (proxyOnce), where the older engine ran it as pass-through, so say so once, here.
    fn report_unknown_proxy_modes(stubs: &[Stub]) {
        for stub in stubs {
            for response in &stub.responses {
                if let StubResponse::Proxy { proxy, .. } = response
                    && let Err(e) = proxy.mode()
                {
                    error!("{e}; this replayed config runs as proxyOnce");
                }
            }
        }
    }
}

mod bitset;
mod cursors;
pub(crate) use cursors::CursorFile;
mod journal_file;
#[cfg(test)]
pub(crate) use journal_file::settle_ticks as journal_settle_ticks;
pub(crate) use journal_file::{Flush, JournalFile, remove_files as journal_file_remove};
mod lifecycle;
mod matching;
pub(crate) use matching::ScenarioStampingStore;
mod stub_index;
use stub_index::StubSnapshot;
mod proxy;
// Clients presenting a `proxy` stub's client certificate (issue #1609).
mod identity_clients;
pub(crate) use identity_clients::IdentityClients;
pub use proxy::{GeneratorDryRun, dry_run_predicate_generators};
pub(crate) use proxy::{ProxiedResponse, ProxyBody, ProxyOutcome, unread_generator_keys};
mod recording;
mod responses;
mod verify;
pub use recording::ClearedProxyResponses;
pub use verify::{ClosestMatch, FailedPredicate, VerifyOptions, VerifyOutcome};

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Issue #1303: positional alignment =====

    fn stub_json(v: serde_json::Value) -> Stub {
        serde_json::from_value(v).expect("stub")
    }

    #[test]
    fn stubs_differing_only_in_responses_match_alike() {
        let a = stub_json(
            serde_json::json!({ "id": "a", "predicates": [{ "equals": { "path": "/x" } }],
            "responses": [{ "is": { "body": "1" } }] }),
        );
        let b = stub_json(
            serde_json::json!({ "id": "b", "predicates": [{ "equals": { "path": "/x" } }],
            "responses": [{ "is": { "body": "2" } }] }),
        );
        assert!(matches_alike(&a, &b));
    }

    #[test]
    fn a_predicate_or_a_scenario_gate_changes_what_a_stub_matches() {
        let base = serde_json::json!({ "predicates": [{ "equals": { "path": "/x" } }],
            "responses": [{ "is": { "body": "1" } }] });
        let a = stub_json(base.clone());
        let mut narrowed = base.clone();
        narrowed["predicates"] = serde_json::json!([{ "equals": { "path": "/y" } }]);
        assert!(!matches_alike(&a, &stub_json(narrowed)));
        let mut gated = base;
        gated["scenarioName"] = serde_json::json!("s");
        gated["requiredScenarioState"] = serde_json::json!("Started");
        assert!(!matches_alike(&a, &stub_json(gated)));
    }

    #[test]
    fn first_divergence_is_the_first_position_that_matches_differently() {
        let on = |path: &str| {
            stub_json(serde_json::json!({
            "predicates": [{ "equals": { "path": path } }], "responses": [{ "is": {} }] }))
        };
        let old = [on("/a"), on("/b"), on("/c")];
        assert_eq!(first_divergence(&old, &old), 3, "identical");
        assert_eq!(first_divergence(&old, &[on("/a"), on("/c")]), 1, "a delete");
        assert_eq!(
            first_divergence(&old, &[on("/b"), on("/a"), on("/c")]),
            0,
            "a move"
        );
        assert_eq!(
            first_divergence(&old, &[on("/a"), on("/b"), on("/c"), on("/d")]),
            3,
            "an append"
        );
        assert_eq!(
            first_divergence(&old, &[on("/a"), on("/b")]),
            2,
            "the last deleted"
        );
    }

    #[test]
    fn restamp_touches_exactly_the_range() {
        let imposter = Imposter::new(
            serde_json::from_value(serde_json::json!({
                "port": 0, "protocol": "http",
                "stubs": [{ "responses": [{ "is": {} }] }, { "responses": [{ "is": {} }] },
                          { "responses": [{ "is": {} }] }]
            }))
            .expect("config"),
        )
        .expect("imposter");
        let mut stubs = imposter.snapshot().stubs().to_vec();
        let before = stubs[0].loaded_at();
        let later = before + chrono::Duration::seconds(5);
        restamp(&mut stubs, 1..=1, later);
        let stamps: Vec<_> = stubs.iter().map(|s| s.loaded_at()).collect();
        assert_eq!(stamps, vec![before, later, before]);
    }

    // ===== Issue #1301: LoadClock =====

    #[test]
    fn a_fresh_load_clock_stamps_the_wall_clock() {
        let clock = LoadClock::new();
        let before = chrono::Utc::now();
        let at = clock.next();
        assert!(at >= before && at <= chrono::Utc::now(), "{at} vs {before}");
        assert_eq!(clock.floor_secs(), at.timestamp());
    }

    #[test]
    fn load_clock_stamps_strictly_increase_at_whole_seconds() {
        let clock = LoadClock::new();
        // A floor ahead of the wall clock stands in for "already stamped this second".
        let ahead = chrono::Utc::now().timestamp() + 5;
        clock.raise_to(ahead);
        let first = clock.next();
        let second = clock.next();
        assert_eq!(first.timestamp(), ahead + 1);
        assert_eq!(second.timestamp(), ahead + 2);
        assert_eq!(
            first.timestamp_subsec_nanos(),
            0,
            "a lead stamp is a whole second"
        );
    }

    #[test]
    fn raising_the_floor_never_lowers_it() {
        let clock = LoadClock::new();
        clock.raise_to(100);
        clock.raise_to(50);
        assert_eq!(clock.floor_secs(), 100);
    }

    #[test]
    fn continuing_an_older_clock_keeps_the_stubs_stamps() {
        let imposter = Imposter::new(
            serde_json::from_value(serde_json::json!({
                "port": 0, "protocol": "http",
                "stubs": [{ "responses": [{ "is": { "body": "a" } }] }]
            }))
            .expect("config"),
        )
        .expect("imposter");
        let before = imposter.snapshot().stubs()[0].loaded_at();
        imposter.continue_load_clock(before.timestamp() - 10);
        assert_eq!(imposter.snapshot().stubs()[0].loaded_at(), before);
    }

    #[test]
    fn continuing_a_newer_clock_restamps_every_stub_past_it() {
        let imposter = Imposter::new(
            serde_json::from_value(serde_json::json!({
                "port": 0, "protocol": "http",
                "stubs": [
                    { "responses": [{ "is": { "body": "a" } }] },
                    { "responses": [{ "is": { "body": "b" } }] }
                ]
            }))
            .expect("config"),
        )
        .expect("imposter");
        let ahead = chrono::Utc::now().timestamp() + 5;
        imposter.continue_load_clock(ahead);
        let snapshot = imposter.snapshot();
        let states = snapshot.stubs();
        assert!(
            states
                .iter()
                .all(|s| s.loaded_at().timestamp() == ahead + 1)
        );
    }
    use crate::imposter::types::ImposterConfig;
    use serde_json::json;

    // Issue #482: removing `pool_max_idle_per_host(0)` (re-enabling connection pooling) must keep
    // the client builder valid. Issue #974 turned the old `.expect` into a returned error, so this
    // now asserts the Ok rather than relying on the absence of a panic.
    #[test]
    fn proxy_http_client_builds() {
        assert!(default_upstream_client().is_ok());
    }

    // Issue #974: constructing an imposter must not realise any TLS trust policy. An imposter
    // with no `proxy` stub never dials an upstream, and an eager build here made every
    // `create_imposter` fail on a host with no CA bundle — including when the operator had
    // supplied a working anchor the manager was about to inject.
    #[test]
    fn constructing_an_imposter_resolves_no_upstream_client() {
        let imposter = make_test_imposter();
        assert!(
            imposter.upstream_client.is_none(),
            "the client must stay unresolved until a proxy stub actually needs it"
        );
    }

    // Issue #974: the process-wide default is shared, not rebuilt per imposter.
    #[test]
    fn default_upstream_client_is_shared() {
        let a = default_upstream_client().expect("first build");
        let b = default_upstream_client().expect("second build");
        assert!(Arc::ptr_eq(&a, &b), "the default client must be reused");
    }

    fn make_test_imposter() -> Imposter {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            ..Default::default()
        };
        Imposter::new(config).expect("test imposter")
    }

    // Issue #966: inert by default — a freshly built imposter (no provider was consulted, since
    // `Imposter::new` never has one) carries no exchange inspector.
    #[test]
    fn fresh_imposter_has_no_exchange_inspector() {
        let imposter = make_test_imposter();
        assert!(imposter.exchange_inspector.is_none());
    }

    // Issue #514: a stub declaring only `scenarioName` (no requiredScenarioState/newScenarioState)
    // is a valid Mountebank config — it names a scenario without gating on it. It must still get a
    // real flow store, or the admin/FFI scenario surface (set-state/list/reset) silently no-ops: the
    // write vanishes into the NoOp store and a read still shows the initial state.
    #[test]
    fn bare_scenario_name_provisions_a_real_flow_store() {
        let cfg: ImposterConfig = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [{
                "scenarioName": "order",
                "responses": [{ "is": { "statusCode": 200 } }]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let flow = imp.resolve_flow_id(&std::collections::HashMap::<String, String>::new());

        imp.set_scenario_state(&flow, "order", "paid")
            .expect("set scenario state");
        assert_eq!(
            imp.scenario_state(&flow, "order")
                .expect("read scenario state"),
            "paid",
            "a bare-scenarioName stub must persist scenario state (real flow store, not NoOp)"
        );
    }

    // Issue #423: stub-analysis warnings are computed once and cached — repeated reads return the
    // SAME Arc (no per-read recompute) — and a stub mutation invalidates the cache so the next read
    // reflects the new stubs.
    #[test]
    fn stub_warnings_cached_until_mutation() {
        use crate::extensions::stub_analysis::WarningType;

        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/dup" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] },
                { "predicates": [{ "equals": { "path": "/dup" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        let a = imp.stub_warnings();
        assert!(
            a.iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate),
            "duplicate stubs must be flagged"
        );
        let b = imp.stub_warnings();
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "repeated reads must return the cached Arc — no recompute on the hot path"
        );

        // Mutating the stubs invalidates the cache; the next read recomputes over the new stubs.
        let distinct: Vec<Stub> = serde_json::from_value(json!([
            { "predicates": [{ "equals": { "path": "/only" } }],
              "responses": [{ "is": { "statusCode": 200, "body": "y" } }] }
        ]))
        .unwrap();
        imp.replace_stubs(distinct);

        let c = imp.stub_warnings();
        assert!(
            !std::sync::Arc::ptr_eq(&a, &c),
            "a stub mutation must invalidate the cached warnings"
        );
        assert!(
            !c.iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate),
            "distinct stubs must have no exact-duplicate warning"
        );
    }

    #[test]
    fn matched_stub_is_shared_arc_not_deep_cloned() {
        // Gate for #287: a match returns the Arc<StubState> stored in the stub vector (a refcount
        // bump), not a deep clone — proven by pointer identity with the stored entry.
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "path": "/shared" } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "x" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let (matched, index) = imp
            .find_matching_stub_with_client("GET", "/shared", &headers, None, None, None, None)
            .expect("store is infallible")
            .expect("request must match");
        let stored = std::sync::Arc::clone(&imp.snapshot().stubs()[index]);
        assert!(
            std::sync::Arc::ptr_eq(&matched, &stored),
            "matched stub must be the shared Arc, not a deep clone"
        );
    }

    // Issue #1463: `flowState.backend` is gone. A config that still names one (including the
    // removed `"redis"` and its block) decodes with the key in `extra`, and gets the in-memory
    // store — no name selects anything any more, so there is nothing to refuse.
    #[test]
    fn a_leftover_backend_key_lands_in_extra_and_gets_the_in_memory_store() {
        for backend in ["inmemory", "redis", "postgres"] {
            let raw = json!({
                "port": 0, "protocol": "http", "stubs": [],
                "_rift": { "flowState": { "backend": backend, "redis": { "url": "redis://localhost:6379" } } }
            });
            let cfg: crate::imposter::ImposterConfig =
                serde_json::from_value(raw).expect("a leftover backend key must still decode");
            let flow_state = cfg
                .rift
                .as_ref()
                .and_then(|r| r.flow_state.as_ref())
                .expect("flowState decodes");
            assert_eq!(
                flow_state.extra.get("backend"),
                Some(&json!(backend)),
                "the stray key must be preserved in extra, not dropped"
            );
            let imp = Imposter::new_with_hooks_and_journal(cfg, None, None, None)
                .expect("a leftover backend key must not fail construction");
            assert_eq!(imp.flow_store.increment("f", "n").expect("incr"), 1);
            assert_eq!(
                imp.flow_store.increment("f", "n").expect("incr"),
                2,
                "backend={backend}: the store must be the in-memory one (NoOp would return 1 again)"
            );
        }
    }

    /// A provider that hands out one shared store and counts how often it was asked.
    struct CountingProvider {
        store: Arc<dyn FlowStore>,
        calls: std::sync::atomic::AtomicUsize,
        supply: bool,
    }

    impl crate::extensions::flow_state::FlowStoreProvider for CountingProvider {
        fn provide(&self, _config: &crate::imposter::ImposterConfig) -> Option<Arc<dyn FlowStore>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.supply.then(|| Arc::clone(&self.store))
        }
    }

    // Issue #1463: the provider is the only store seam. A store it supplies wins over an
    // explicit `_rift.flowState`, and is consulted exactly once per construction.
    #[test]
    fn a_provider_store_overrides_an_explicit_flow_state() {
        let shared: Arc<dyn FlowStore> = Arc::new(crate::backends::InMemoryFlowStore::new(60));
        shared
            .set("f", "seed", json!("from-provider"))
            .expect("seed");
        let provider = Arc::new(CountingProvider {
            store: Arc::clone(&shared),
            calls: std::sync::atomic::AtomicUsize::new(0),
            supply: true,
        });
        let as_dyn: Arc<dyn crate::extensions::flow_state::FlowStoreProvider> = provider.clone();
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "ttlSeconds": 300 } }
        }))
        .expect("valid imposter config");

        let imp = Imposter::new_with_hooks_and_journal(cfg, Some(&as_dyn), None, None)
            .expect("a provider store must build");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the provider must be consulted exactly once"
        );
        assert_eq!(
            imp.flow_store.get("f", "seed").expect("get"),
            Some(json!("from-provider")),
            "the imposter must use the provider's store, not a fresh in-memory one"
        );
    }

    // A provider that declines (`None`) defers to the built-in selection: `_rift.flowState`
    // still gets its own in-memory store.
    #[test]
    fn a_declining_provider_defers_to_the_in_memory_store() {
        let provider = Arc::new(CountingProvider {
            store: Arc::new(crate::extensions::flow_state::NoOpFlowStore),
            calls: std::sync::atomic::AtomicUsize::new(0),
            supply: false,
        });
        let as_dyn: Arc<dyn crate::extensions::flow_state::FlowStoreProvider> = provider.clone();
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "ttlSeconds": 300 } }
        }))
        .expect("valid imposter config");

        let imp = Imposter::new_with_hooks_and_journal(cfg, Some(&as_dyn), None, None)
            .expect("a declining provider must not fail construction");
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(imp.flow_store.increment("f", "n").expect("incr"), 1);
        assert_eq!(
            imp.flow_store.increment("f", "n").expect("incr"),
            2,
            "the fallback must be the in-memory store, not the provider's declined NoOp"
        );
    }

    // Issue #358: a `_rift.script` stub might call `ctx.state`/`flow_store` at runtime — without
    // `_rift.flowState` configured it must get a REAL in-memory store, not the silent NoOp (state
    // must persist across calls; NoOp's `increment` always returns 1).
    #[test]
    fn script_stub_without_flow_state_auto_provisions_in_memory() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "is": { "statusCode": 200 },
                    "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { pass() }" } }
                }]
            }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(
            imp.flow_store.increment("f", "attempts").expect("incr"),
            2,
            "NoOpFlowStore would return 1 both times; the auto-provisioned in-memory store must persist"
        );
    }

    // Same guarantee for the script-only `RiftScript` response variant (no `is` wrapper).
    #[test]
    fn script_only_response_without_flow_state_auto_provisions_in_memory() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "responses": [{
                    "_rift": { "script": { "engine": "rhai", "code": "fn respond(ctx) { http(200) }" } }
                }]
            }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 2);
    }

    // Issue #1504: the admin API and FFI write the store outside request matching, so even an
    // imposter with no state surface needs a real store.
    #[test]
    fn plain_stub_without_flow_state_gets_in_memory_store() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "is": { "statusCode": 200 } }] }]
        }))
        .expect("valid imposter config");
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 1);
        assert_eq!(imp.flow_store.increment("f", "attempts").expect("incr"), 2);
    }

    #[test]
    fn implicit_store_construction_succeeds_silently() {
        // No _rift.flowState and no scenario stubs: the implicit store must not be an error.
        let cfg = serde_json::from_value(json!({ "port": 0, "protocol": "http", "stubs": [] }))
            .expect("valid imposter config");
        assert!(Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_ok());
    }

    #[test]
    fn explicit_flow_state_construction_succeeds() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": {} }
        }))
        .expect("valid imposter config");
        assert!(Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_ok());
    }

    // Issue #530: a non-positive flowState.ttlSeconds misbehaves late (instant expiry / SETEX
    // error), so it must fail construction up front — surfacing as a 400 at imposter creation.
    #[test]
    fn non_positive_ttl_seconds_fails_construction() {
        for ttl in [0, -1] {
            let cfg = serde_json::from_value(json!({
                "port": 0, "protocol": "http", "stubs": [],
                "_rift": { "flowState": { "ttlSeconds": ttl } }
            }))
            .expect("valid imposter config");
            let msg = match Imposter::new_with_hooks_and_journal(cfg, None, None, None) {
                Ok(_) => panic!("ttlSeconds = {ttl} must fail construction"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains("ttlSeconds"),
                "error must name the offending field, got: {msg}"
            );
        }
    }

    // A ttlSeconds of exactly 1 (the boundary) is accepted.
    #[test]
    fn ttl_seconds_one_is_accepted() {
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [],
            "_rift": { "flowState": { "ttlSeconds": 1 } }
        }))
        .expect("valid imposter config");
        assert!(Imposter::new_with_hooks_and_journal(cfg, None, None, None).is_ok());
    }

    /// Serve the state's next response body, advancing the shared cycler.
    fn served_body(state: &StubState) -> String {
        let resp = state.get_next_response().expect("stub has responses");
        serde_json::to_value(resp).expect("serialize")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    #[test]
    fn replace_stub_restarts_the_response_cycle() {
        // #1515: a replace by index is Mountebank's `overwriteAtIndex`, which rewraps the stub, so
        // the new content starts its cycle over (it used to inherit the old cursor, #287).
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.snapshot().stubs()[0]), "A"); // cursor now at index 1
        let new = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "C" } },
                { "is": { "statusCode": 200, "body": "D" } }
            ]
        }))
        .unwrap();
        imp.replace_stub(0, new).expect("index in bounds");
        assert_eq!(
            served_body(&imp.snapshot().stubs()[0]),
            "C",
            "cycle restarted"
        );
    }

    #[test]
    fn replace_stub_with_identical_content_restarts_the_cycle() {
        let stub = json!({
            "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "A" } },
                { "is": { "statusCode": 200, "body": "B" } },
                { "is": { "statusCode": 200, "body": "C" } }
            ]
        });
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http", "stubs": [stub.clone()]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        let before = imp.snapshot().stubs()[0].clone();
        assert_eq!(served_body(&before), "A");
        assert_eq!(served_body(&before), "B");
        imp.replace_stub(0, serde_json::from_value(stub).unwrap())
            .expect("index in bounds");
        let after = imp.snapshot().stubs()[0].clone();
        assert_ne!(
            after.slot, before.slot,
            "a new slot, so slot-keyed cursors restart"
        );
        assert_eq!(
            after.loaded_at(),
            before.loaded_at(),
            "#1294: identical content keeps its stamp"
        );
        let served: Vec<String> = (0..3).map(|_| served_body(&after)).collect();
        assert_eq!(served, ["A", "B", "C"], "Mountebank: `A B · A B C`");
    }

    #[test]
    fn replace_stub_by_id_preserves_response_cursor() {
        // Same guarantee for the id-based in-place replace (#287).
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "id": "s1",
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.snapshot().stubs()[0]), "A"); // cursor now at index 1
        let new = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/c" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "C" } },
                { "is": { "statusCode": 200, "body": "D" } }
            ]
        }))
        .unwrap();
        assert!(imp.replace_stub_by_id("s1", new), "id exists");
        assert_eq!(
            served_body(&imp.snapshot().stubs()[0]),
            "D",
            "cursor (index 1) preserved and content swapped"
        );
    }

    #[test]
    fn proxy_always_append_preserves_cursor_and_slot() {
        // Gate for #287: the proxyAlways append branch rebuilds the entry via `with_stub`, so
        // the appended-to stub keeps its slot token and response cursor rather than getting a
        // fresh StubState.
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "responses": [{ "proxy": { "to": "http://upstream", "mode": "proxyAlways" } }] }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        // First record → inserted after the proxy stub (insert branch), 2 responses.
        let rec = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/rec" } }],
            "responses": [
                { "is": { "statusCode": 200, "body": "R1" } },
                { "is": { "statusCode": 200, "body": "R2" } }
            ]
        }))
        .unwrap();
        imp.insert_or_append_proxy_stub(rec, "http://upstream", "proxyAlways", None, None);

        let rec_index = imp
            .snapshot()
            .stubs()
            .iter()
            .position(|s| !s.stub.predicates.is_empty())
            .expect("recorded stub present");
        let slot_before = imp.snapshot().stubs()[rec_index].slot;
        assert_eq!(served_body(&imp.snapshot().stubs()[rec_index]), "R1"); // cursor now at index 1

        // Second record with identical predicates → append branch: responses become [R1, R2, R3].
        let rec2 = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/rec" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "R3" } }]
        }))
        .unwrap();
        imp.insert_or_append_proxy_stub(rec2, "http://upstream", "proxyAlways", None, None);

        assert_eq!(
            imp.snapshot().stubs()[rec_index].slot,
            slot_before,
            "append must reuse the slot token, not mint a fresh StubState"
        );
        assert_eq!(
            served_body(&imp.snapshot().stubs()[rec_index]),
            "R2",
            "cursor (index 1) preserved across the append"
        );
    }

    #[test]
    fn concurrent_writers_no_lost_update() {
        // Gate for #291: writers RCU under a serializing mutex, so N concurrent `add_stub` calls
        // all land. A naive load→clone→mutate→store without serialization would lose updates
        // (two writers clone the same snapshot; the second store clobbers the first).
        use std::sync::Arc as StdArc;
        let imp = StdArc::new(make_test_imposter());
        let n = 32usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let imp = StdArc::clone(&imp);
                std::thread::spawn(move || {
                    let stub: Stub = serde_json::from_value(json!({
                        "id": format!("s{i}"),
                        "responses": [{ "is": { "statusCode": 200, "body": "x" } }]
                    }))
                    .unwrap();
                    imp.add_stub(stub, None);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            imp.get_stubs().len(),
            n,
            "every concurrent add must be retained (no lost update)"
        );
    }

    #[test]
    fn cursor_survives_unrelated_structural_mutation() {
        // Gate for #291: an unrelated structural mutation (adding another stub) rebuilds the whole
        // stub-vector snapshot. The untouched stub's `Arc<StubState>` must be carried into the new
        // snapshot by reference (Arc clone), so its response cursor is preserved — a deep copy of
        // the state would reset it.
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{
                "predicates": [{ "equals": { "path": "/c" } }],
                "responses": [
                    { "is": { "statusCode": 200, "body": "A" } },
                    { "is": { "statusCode": 200, "body": "B" } }
                ]
            }]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");
        assert_eq!(served_body(&imp.snapshot().stubs()[0]), "A"); // cursor now at index 1
        let other: Stub = serde_json::from_value(json!({
            "predicates": [{ "equals": { "path": "/other" } }],
            "responses": [{ "is": { "statusCode": 200, "body": "Z" } }]
        }))
        .unwrap();
        imp.add_stub(other, None);
        assert_eq!(
            served_body(&imp.snapshot().stubs()[0]),
            "B",
            "cursor preserved across an unrelated structural snapshot swap (Arc identity carried)"
        );
    }

    #[test]
    fn concurrent_reads_during_swap_consistent() {
        // Gate for #291: readers on the match path load a consistent, wait-free snapshot while a
        // writer swaps the entire stub set. The path always has a matching stub, so the reader
        // must keep matching and must never tear or panic.
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let cfg = serde_json::from_value(json!({
            "port": 0, "protocol": "http",
            "stubs": [{ "predicates": [{ "equals": { "path": "/p" } }],
                        "responses": [{ "is": { "statusCode": 200, "body": "v0" } }] }]
        }))
        .unwrap();
        let imp = StdArc::new(Imposter::new(cfg).expect("test imposter"));
        let stop = StdArc::new(AtomicBool::new(false));
        let reader = {
            let imp = StdArc::clone(&imp);
            let stop = StdArc::clone(&stop);
            std::thread::spawn(move || {
                let headers: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                let mut hits = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let r = imp
                        .find_matching_stub_with_client(
                            "GET", "/p", &headers, None, None, None, None,
                        )
                        .expect("store infallible");
                    if r.is_some() {
                        hits += 1;
                    }
                }
                hits
            })
        };
        for i in 1..500u32 {
            let stub: Stub = serde_json::from_value(json!({
                "predicates": [{ "equals": { "path": "/p" } }],
                "responses": [{ "is": { "statusCode": 200, "body": format!("v{i}") } }]
            }))
            .unwrap();
            imp.replace_stubs(vec![stub]);
        }
        stop.store(true, Ordering::Release);
        let hits = reader.join().unwrap();
        assert!(
            hits > 0,
            "reader saw a consistent matching snapshot throughout the swaps"
        );
    }

    // Fix #95, #1553: a repeated form field keeps every value instead of being overwritten
    #[test]
    fn test_parse_form_data_multi_valued_fields() {
        // parse_form_data now reads the pre-built header map (Title-Case keys); the Content-Type
        // lookup must remain case-insensitive.
        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );

        let result = Imposter::parse_form_data(&headers, Some("checkbox=A&checkbox=B&checkbox=C"));
        let form = result.expect("Should parse form data");
        assert_eq!(
            form.get("checkbox").unwrap(),
            &["A", "B", "C"],
            "a repeated form field keeps every value in order"
        );

        // Content-Type lookup must be case-insensitive over the map (a lower-cased key must
        // still be recognized), matching the old case-insensitive HeaderMap::get.
        let mut lower: HashMap<String, String> = HashMap::new();
        lower.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        assert!(
            Imposter::parse_form_data(&lower, Some("a=1")).is_some(),
            "a differently-cased content-type key must still be recognized"
        );
    }

    #[test]
    fn find_matching_stub_with_client_matches_on_prebuilt_header_map() {
        // Gate for #288: the matcher takes the already-built header HashMap (no re-conversion
        // from HeaderMap) and header predicates still match against it correctly.
        let cfg = serde_json::from_value(json!({
            "port": 0,
            "protocol": "http",
            "stubs": [
                { "predicates": [{ "equals": { "headers": { "X-Api-Key": "secret" } } }],
                  "responses": [{ "is": { "statusCode": 200, "body": "ok" } }] }
            ]
        }))
        .unwrap();
        let imp = Imposter::new(cfg).expect("test imposter");

        let mut headers: HashMap<String, String> = HashMap::new();
        headers.insert("X-Api-Key".to_string(), "secret".to_string());
        let matched = imp
            .find_matching_stub_with_client("GET", "/", &headers, None, None, None, None)
            .expect("store is infallible");
        assert!(matched.is_some(), "request with matching header must match");

        let mut wrong: HashMap<String, String> = HashMap::new();
        wrong.insert("X-Api-Key".to_string(), "nope".to_string());
        let unmatched = imp
            .find_matching_stub_with_client("GET", "/", &wrong, None, None, None, None)
            .expect("store is infallible");
        assert!(
            unmatched.is_none(),
            "request with non-matching header must not match"
        );
    }

    // Issue #1329: a generator's `caseSensitive` is copied onto each predicate only when present,
    // as in Mountebank. Absent, the predicate default (`false`) applies, as it does in Mountebank.
    #[test]
    fn test_generator_omits_absent_case_sensitive() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "method": true, "path": true } })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/API/Users",
                &crate::imposter::Headers::new(),
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![
                json!({ "deepEquals": { "method": "GET" } }),
                json!({ "deepEquals": { "path": "/API/Users" } }),
            ]
        );
    }

    #[test]
    fn test_generator_copies_a_present_case_sensitive() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "path": true }, "caseSensitive": true })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/A",
                &crate::imposter::Headers::new(),
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "path": "/A" }, "caseSensitive": true })]
        );
    }

    // Issue #1329: `except` is persisted on the predicate and the captured value stays raw — the
    // matcher strips both sides, as in Mountebank. Stripping at generation and dropping the pattern
    // recorded stubs no later request could match.
    #[test]
    fn test_generator_persists_except_for_method() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "method": true }, "except": "^POST$" })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "POST",
                "/test",
                &crate::imposter::Headers::new(),
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "method": "POST" }, "except": "^POST$" })]
        );
    }

    #[test]
    fn test_generator_persists_except_for_path() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "path": true }, "except": r"\d+$" })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/orders/123",
                &crate::imposter::Headers::new(),
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "path": "/orders/123" }, "except": r"\d+$" })]
        );
    }

    #[test]
    fn test_generator_persists_except_for_body() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "body": true }, "except": r"\d+" })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "POST",
                "/test",
                &crate::imposter::Headers::new(),
                Some("token=abc123"),
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![json!({ "deepEquals": { "body": "token=abc123" }, "except": r"\d+" })]
        );
    }

    // Issue #1329: the predicate a generator records with `except` matches a later request that
    // differs only in what `except` strips — and the recorded request itself.
    #[test]
    fn test_generated_except_predicate_matches_again() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "path": true }, "except": r"\d+" })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/users/123",
                &crate::imposter::Headers::new(),
                None,
                None,
            )
            .expect("predicate generation succeeds");
        let predicates: Vec<crate::imposter::types::Predicate> = predicates
            .into_iter()
            .map(|p| serde_json::from_value(p).expect("generated predicate loads"))
            .collect();
        let headers: HashMap<String, String> = HashMap::new();
        let matches = |path: &str| {
            predicates.iter().all(|p| {
                crate::imposter::predicates::predicate_matches(
                    p, "GET", path, None, &headers, None, None, None, None, 0,
                )
                .expect("evaluates")
            })
        };
        assert!(matches("/users/123"), "the recorded request matches again");
        assert!(
            matches("/users/456"),
            "a request differing only in `except` matches"
        );
        assert!(!matches("/orders/1"));
    }

    // Fix #109, reshaped by #1329: one predicate per matched field, query as `deepEquals`.
    #[test]
    fn test_generator_includes_query_parameters() {
        let imposter = make_test_imposter();
        let generators = vec![json!({ "matches": { "path": true, "query": true } })];
        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/search",
                &crate::imposter::Headers::new(),
                None,
                Some("q=hello&page=1"),
            )
            .expect("predicate generation succeeds");

        assert_eq!(
            predicates,
            vec![
                json!({ "deepEquals": { "path": "/search" } }),
                json!({ "deepEquals": { "query": { "q": "hello", "page": "1" } } }),
            ]
        );
    }

    // =========================================================================
    // Gap 5.2: predicateGenerators.inject — JS function produces predicates
    // =========================================================================

    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_produces_predicates() {
        let imposter = make_test_imposter();

        let inject_fn = r#"function(config, logger, predicates) {
            return [{ equals: { path: config.request.path } }];
        }"#;

        let generators = vec![json!({ "inject": inject_fn })];
        let headers = crate::imposter::Headers::new();

        let predicates = imposter
            .generate_predicates_from_request(
                &generators,
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect("predicate generation succeeds");

        assert_eq!(predicates.len(), 1);
        let equals = predicates[0].get("equals").expect("should have equals key");
        assert_eq!(equals["path"], "/api/users");
    }

    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_receives_existing_predicates() {
        let imposter = make_test_imposter();

        // First generator builds a "method" predicate via matches, second via inject
        let inject_fn = r#"function(config, logger, predicates) {
            var result = predicates.slice();
            result.push({ equals: { path: config.request.path } });
            return result;
        }"#;

        let generators = vec![
            json!({ "matches": { "method": true } }),
            json!({ "inject": inject_fn }),
        ];
        let headers = crate::imposter::Headers::new();

        let predicates = imposter
            .generate_predicates_from_request(&generators, "POST", "/orders", &headers, None, None)
            .expect("predicate generation succeeds");

        // matches generator produces 1, inject generator returns 2 (original + new path)
        assert_eq!(predicates.len(), 3);
    }

    // Issue #498: a failing inject generator must be DISTINGUISHABLE from one that legitimately
    // produced no predicates. A script error surfaces as `Err` (so the caller can skip auto-stub
    // creation), while an inject that returns `[]` is `Ok(empty)` (a real, intended empty list).
    #[cfg(feature = "javascript")]
    #[test]
    fn generator_inject_failure_is_distinguishable_from_empty() {
        let imposter = make_test_imposter();
        let headers = crate::imposter::Headers::new();

        // A throwing inject → Err, NOT an empty predicate list.
        let throwing = r#"function(config, logger, predicates) { throw new Error("boom"); }"#;
        let err = imposter
            .generate_predicates_from_request(
                &[json!({ "inject": throwing })],
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect_err("a throwing generator must fail, not silently return empty predicates");
        assert_eq!(err.kind(), "script-error");

        // An inject that explicitly returns [] is a legitimate empty result → Ok(empty).
        let empty = r#"function(config, logger, predicates) { return []; }"#;
        let preds = imposter
            .generate_predicates_from_request(
                &[json!({ "inject": empty })],
                "GET",
                "/api/users",
                &headers,
                None,
                None,
            )
            .expect("an inject returning [] is a valid empty result, not a failure");
        assert!(preds.is_empty());
    }

    #[test]
    fn test_record_request_cap_enforced() {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            record_requests: true,
            ..Default::default()
        };
        // A budget of exactly 100 of the entries below (issue #1441: bytes, not a count).
        let budget = 100;
        let req = RecordedRequest {
            body_omitted: false,
            mode: ResponseMode::Text,
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: "/".to_string(),
            query: Default::default(),
            headers: crate::imposter::Headers::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        };

        let per_entry = crate::imposter::journal::entry_cost(&req);
        let journal = Arc::new(crate::imposter::journal::LocalJournal::with_max_bytes(
            std::num::NonZeroUsize::new(budget * per_entry).expect("non-zero"),
        ));
        let imposter = Imposter::new_with_hooks_and_journal(config, None, None, Some(journal))
            .expect("test imposter");
        for _ in 0..budget + 10 {
            imposter.record_request(req.clone());
        }

        let recorded = imposter.get_recorded_requests();
        assert_eq!(
            recorded.len(),
            budget,
            "Recorded requests must not exceed the byte budget"
        );
    }

    // Issue #201: a targeted retain removes only the non-kept entries and, unlike a
    // full clear, must NOT reset the total request count (other slices' requests remain).
    #[test]
    fn test_retain_recorded_requests_preserves_count() {
        let config = ImposterConfig {
            port: Some(0),
            protocol: "http".to_string(),
            record_requests: true,
            ..Default::default()
        };
        let imposter = Imposter::new(config).expect("test imposter");

        let req = |space: &str| {
            let mut headers = crate::imposter::Headers::new();
            headers.insert("X-Mock-Space".to_string(), vec![space.to_string()]);
            RecordedRequest {
                body_omitted: false,
                mode: ResponseMode::Text,
                request_from: "127.0.0.1".to_string(),
                method: "GET".to_string(),
                path: "/".to_string(),
                query: Default::default(),
                headers,
                body: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                match_outcome: None,
                status: None,
                latency_ms: None,
                node: None,
            }
        };
        imposter.record_request(req("A"));
        imposter.record_request(req("B"));
        imposter.record_request(req("A"));
        for _ in 0..3 {
            imposter.increment_request_count();
        }

        imposter.retain_recorded_requests(|r| {
            r.headers
                .get("X-Mock-Space")
                .and_then(|v| v.first())
                .map(String::as_str)
                != Some("A")
        });
        assert_eq!(imposter.get_recorded_requests().len(), 1, "only B kept");
        assert_eq!(
            imposter.get_request_count(),
            3,
            "targeted retain must not reset the request count"
        );

        imposter.clear_recorded_requests().expect("clear");
        assert_eq!(imposter.get_recorded_requests().len(), 0);
        assert_eq!(
            imposter.get_request_count(),
            0,
            "full clear resets the request count"
        );
    }

    // Issue #498: a malformed inject generator (here, a non-function body) must FAIL rather than
    // silently return an empty predicate list — the empty list is what produced the match-all stub.
    #[cfg(feature = "javascript")]
    #[test]
    fn test_generator_inject_bad_function_returns_err() {
        let imposter = make_test_imposter();

        let generators = vec![json!({ "inject": "not a function" })];
        let headers = crate::imposter::Headers::new();

        let err = imposter
            .generate_predicates_from_request(&generators, "GET", "/test", &headers, None, None)
            .expect_err("a malformed inject generator must fail, not return empty predicates");
        assert_eq!(err.kind(), "script-error");
    }

    // Issue #1254: a reconcile that changes nothing must not rebuild the snapshot and its match
    // index; neither must a degenerate one, which the manager answers with a wholesale replace.
    mod reconcile_snapshot {
        use super::*;
        use crate::imposter::reconcile::StubReconcile;

        fn stubs(bodies: &[&str]) -> Vec<Stub> {
            bodies
                .iter()
                .map(|body| {
                    serde_json::from_value(json!({
                        "predicates": [{ "equals": { "method": "GET", "path": format!("/{body}") } }],
                        "responses": [{ "is": { "statusCode": 200, "headers": { "A": "1", "B": "2" }, "body": body } }]
                    }))
                    .expect("stub json")
                })
                .collect()
        }

        fn imposter_with(bodies: &[&str]) -> Imposter {
            let config: ImposterConfig =
                serde_json::from_value(json!({ "port": 0, "protocol": "http" })).expect("config");
            let imposter = Imposter::new(config).expect("imposter");
            imposter.replace_stubs(stubs(bodies));
            imposter
        }

        #[test]
        fn an_unchanged_reconcile_keeps_the_snapshot() {
            let imposter = imposter_with(&["a", "b", "c"]);
            let before = imposter.stubs_snapshot.load_full();
            let outcome = imposter.reconcile_stubs(&stubs(&["a", "b", "c"]));
            assert!(matches!(outcome, StubReconcile::Unchanged), "{outcome:?}");
            assert!(Arc::ptr_eq(&before, &imposter.stubs_snapshot.load_full()));
        }

        #[test]
        fn a_degenerate_reconcile_keeps_the_snapshot() {
            let imposter = imposter_with(&["a", "b"]);
            let before = imposter.stubs_snapshot.load_full();
            let outcome = imposter.reconcile_stubs(&stubs(&["x", "y"]));
            assert!(matches!(outcome, StubReconcile::Degenerate), "{outcome:?}");
            assert!(Arc::ptr_eq(&before, &imposter.stubs_snapshot.load_full()));
        }

        #[test]
        fn a_patch_stores_a_new_snapshot_reusing_unchanged_states() {
            let imposter = imposter_with(&["a", "b", "c", "d"]);
            let before = imposter.stubs_snapshot.load_full();
            let outcome = imposter.reconcile_stubs(&stubs(&["a", "b", "c", "e"]));
            assert!(
                matches!(outcome, StubReconcile::Patched { .. }),
                "{outcome:?}"
            );
            let after = imposter.stubs_snapshot.load_full();
            assert!(!Arc::ptr_eq(&before, &after));
            assert!(Arc::ptr_eq(&before.stubs()[0], &after.stubs()[0]));
        }

        #[test]
        fn the_cached_hash_and_key_match_the_stub() {
            let [first, second]: [Stub; 2] = stubs(&["a", "b"]).try_into().expect("two stubs");
            let state = StubState::new(first.clone());
            assert_eq!(
                state.content_hash(),
                crate::imposter::reconcile::content_hash(&first)
            );
            assert_eq!(
                state.sequence_key(),
                crate::imposter::reconcile::stub_key(&first, 0)
            );
            let swapped = state.with_stub(second.clone());
            assert_eq!(
                swapped.content_hash(),
                crate::imposter::reconcile::content_hash(&second),
                "an in-place replace must not keep the old stub's hash"
            );
            assert_eq!(StubState::with_hash(first, 42).content_hash(), 42);
        }
    }

    /// Issue #1513: a `proxyAlways` append joins the tail of the stub's rotating queue, as in
    /// Mountebank's default (in-memory) repository. Expected sequences are mb 2.9.5's, probed live.
    mod proxy_always_queue {
        use super::*;
        use proptest::prelude::*;
        use std::collections::VecDeque;

        fn response(label: &str, repeat: u32) -> StubResponse {
            let mut value = json!({ "is": { "statusCode": 200, "body": label } });
            if repeat > 1 {
                value["_behaviors"] = json!({ "repeat": repeat });
            }
            serde_json::from_value(value).expect("response")
        }

        fn state(responses: &[(&str, u32)]) -> StubState {
            let stub: Stub =
                serde_json::from_value(json!({ "responses": [{ "is": {} }] })).expect("stub");
            let mut state = StubState::new(stub);
            state.stub.responses = responses.iter().map(|(l, r)| response(l, *r)).collect();
            state
        }

        fn append(state: &StubState, label: &str) -> StubState {
            state.with_appended(vec![response(label, 1)], state.cursor(), true)
        }

        fn labels(state: &StubState) -> Vec<String> {
            state
                .stub
                .responses
                .iter()
                .map(|r| serde_json::to_value(r).expect("serialize")["is"]["body"].to_string())
                .map(|s| s.trim_matches('"').to_string())
                .collect()
        }

        /// Run `script` (`"+"` appends `U<n>`, anything else is one GET) and return what was
        /// served, `+` marking each append, as the probe table writes it.
        fn run(start: &[(&str, u32)], script: &str) -> (StubState, String) {
            let mut state = state(start);
            let mut out = Vec::new();
            let mut appended = 0;
            for step in script.split_whitespace() {
                if step == "+" {
                    state = append(&state, &format!("U{appended}"));
                    appended += 1;
                    out.push("+".to_string());
                } else {
                    out.push(served_body(&state));
                }
            }
            (state, out.join(" "))
        }

        #[test]
        fn s1_mid_cycle_append_joins_the_queue_tail() {
            let (state, served) = run(&[("A", 1), ("B", 1), ("C", 1)], "g + g g g g g");
            assert_eq!(served, "A + B C A U0 B");
            assert_eq!(
                labels(&state),
                ["A", "B", "C", "U0"],
                "display stays append order"
            );
        }

        #[test]
        fn s2_append_inside_a_repeat_run_splits_it() {
            let (_, served) = run(&[("A", 2), ("B", 1)], "g + g g g g g g g g");
            assert_eq!(served, "A + A B A U0 A B A U0");
        }

        #[test]
        fn s3_append_mid_repeat_of_a_later_response() {
            let (_, served) = run(&[("A", 1), ("B", 3), ("C", 1)], "g g g + g g g g g g g g");
            assert_eq!(served, "A B B + B C A B B U0 B C");
        }

        #[test]
        fn s4_consecutive_appends() {
            let (state, served) = run(&[("A", 1), ("B", 1), ("C", 1)], "g + g + g g g g g g");
            assert_eq!(served, "A + B + C A U0 B U1 C");
            assert_eq!(labels(&state), ["A", "B", "C", "U0", "U1"]);
        }

        #[test]
        fn s5_append_at_the_start_of_a_cycle_keeps_the_display_order() {
            let (state, served) = run(&[("A", 1), ("B", 1), ("C", 1)], "g g g + g g g g g");
            assert_eq!(served, "A B C + A B C U0 A");
            assert!(state.cycle().is_none(), "no order for an append at (0, 0)");
        }

        #[test]
        fn record_then_replay_never_builds_an_order() {
            let (state, served) = run(&[("A", 1)], "+ + g g g");
            assert_eq!(served, "+ + A U0 U1");
            assert!(state.cycle().is_none());
        }

        #[test]
        fn new_content_drops_the_order_and_keeps_the_next_response() {
            // After S1's append and one more GET the queue is C A U0 B: the next answer is C.
            let (state, _) = run(&[("A", 1), ("B", 1), ("C", 1)], "g + g");
            assert!(state.cycle().is_some());
            let replaced = state.with_stub(state.stub.clone());
            assert!(replaced.cycle().is_none());
            let served: Vec<String> = (0..4).map(|_| served_body(&replaced)).collect();
            assert_eq!(served, ["C", "U0", "A", "B"], "display order from C on");
        }

        #[test]
        fn a_cursor_past_the_end_appends_after_the_last_response() {
            // The stub shrank under its cursor: the cycler clamps, serving the last response.
            let state = state(&[("A", 1), ("B", 1)]);
            state.cycler.set(5);
            let appended = append(&state, "U0");
            let served: Vec<String> = (0..4).map(|_| served_body(&appended)).collect();
            assert_eq!(served, ["B", "A", "U0", "B"]);
        }

        /// Delegates to `LocalSequencer`, recording each `next`'s `(response_count, repeats)`.
        #[derive(Default)]
        struct Counting {
            inner: crate::behaviors::LocalSequencer,
            shapes: parking_lot::Mutex<Vec<(usize, Vec<u32>)>>,
        }

        impl crate::behaviors::ResponseSequencer for Counting {
            fn next(
                &self,
                key: crate::behaviors::SequenceKey<'_>,
                count: usize,
                repeats: &[u32],
            ) -> anyhow::Result<usize> {
                self.shapes.lock().push((count, repeats.to_vec()));
                self.inner.next(key, count, repeats)
            }
            fn peek(
                &self,
                key: crate::behaviors::SequenceKey<'_>,
                count: usize,
                repeats: &[u32],
            ) -> anyhow::Result<usize> {
                self.inner.peek(key, count, repeats)
            }
            fn reset_scope(&self, port: u16, stub_key: Option<&str>) {
                self.inner.reset_scope(port, stub_key);
            }
        }

        #[test]
        fn a_sequencer_serves_the_queue_order_over_cycle_positions() {
            let sequencer = Arc::new(Counting::default());
            let cfg = serde_json::from_value(json!({
                "port": 0, "protocol": "http",
                "stubs": [
                    {
                        "predicates": [{ "equals": { "headers": { "x-rec": "1" } } }],
                        "responses": [{ "proxy": { "to": "http://up", "mode": "proxyAlways" } }]
                    },
                    {
                        "predicates": [{ "deepEquals": { "path": "/b" } }],
                        "responses": [
                            { "is": { "body": "A" } },
                            { "is": { "body": "B" } },
                            { "is": { "body": "C" } }
                        ]
                    }
                ]
            }))
            .expect("config");
            let imposter = Imposter::new_with_hooks(cfg, None, Some(Arc::clone(&sequencer) as _))
                .expect("imposter");
            let next = || {
                let snapshot = imposter.snapshot();
                let response = imposter
                    .next_stub_response(&snapshot.stubs()[1])
                    .expect("sequencer")
                    .expect("a response");
                serde_json::to_value(response).expect("serialize")["is"]["body"]
                    .as_str()
                    .expect("body")
                    .to_string()
            };
            let recording = serde_json::from_value(json!({
                "predicates": [{ "deepEquals": { "path": "/b" } }],
                "responses": [{ "is": { "body": "U0" } }]
            }))
            .expect("recording");

            let mut served = vec![next()];
            imposter.insert_or_append_proxy_stub(recording, "http://up", "proxyAlways", None, None);
            served.extend((0..5).map(|_| next()));
            assert_eq!(served, ["A", "B", "C", "A", "U0", "B"]);
            assert_eq!(
                sequencer.shapes.lock().last(),
                Some(&(4, vec![1, 1, 1, 1])),
                "after the append the sequencer counts cycle positions"
            );
        }

        /// A sequencer keyed by `stub_key` alone, as a cross-node backend keys it; repeats ignored.
        #[derive(Default)]
        struct ByStubKey {
            cursors: parking_lot::Mutex<HashMap<String, usize>>,
        }

        impl crate::behaviors::ResponseSequencer for ByStubKey {
            fn next(
                &self,
                key: crate::behaviors::SequenceKey<'_>,
                count: usize,
                _repeats: &[u32],
            ) -> anyhow::Result<usize> {
                let mut cursors = self.cursors.lock();
                let cursor = cursors.entry(key.stub_key.to_string()).or_insert(0);
                let index = *cursor % count;
                *cursor = (index + 1) % count;
                Ok(index)
            }
            fn peek(
                &self,
                key: crate::behaviors::SequenceKey<'_>,
                count: usize,
                _repeats: &[u32],
            ) -> anyhow::Result<usize> {
                Ok(self.cursors.lock().get(key.stub_key).copied().unwrap_or(0) % count)
            }
            fn reset_scope(&self, _port: u16, stub_key: Option<&str>) {
                match stub_key {
                    Some(key) => self.cursors.lock().remove(key),
                    None => {
                        self.cursors.lock().clear();
                        None
                    }
                };
            }
        }

        /// An imposter whose stub 1 (`id` "s", `[A, B, C]`) is merged into by a `proxyAlways`
        /// recording of `/b`, and a `next` that serves stub 1.
        fn sequenced(sequencer: Arc<dyn crate::behaviors::ResponseSequencer>) -> (Imposter, Stub) {
            let cfg = serde_json::from_value(json!({
                "port": 0, "protocol": "http",
                "stubs": [
                    {
                        "predicates": [{ "equals": { "headers": { "x-rec": "1" } } }],
                        "responses": [{ "proxy": { "to": "http://up", "mode": "proxyAlways" } }]
                    },
                    {
                        "id": "s",
                        "predicates": [{ "deepEquals": { "path": "/b" } }],
                        "responses": [
                            { "is": { "body": "A" } },
                            { "is": { "body": "B" } },
                            { "is": { "body": "C" } }
                        ]
                    }
                ]
            }))
            .expect("config");
            let recording = serde_json::from_value(json!({
                "predicates": [{ "deepEquals": { "path": "/b" } }],
                "responses": [{ "is": { "body": "U0" } }]
            }))
            .expect("recording");
            (
                Imposter::new_with_hooks(cfg, None, Some(sequencer)).expect("imposter"),
                recording,
            )
        }

        fn next_of(imposter: &Imposter) -> String {
            let snapshot = imposter.snapshot();
            let response = imposter
                .next_stub_response(&snapshot.stubs()[1])
                .expect("sequencer")
                .expect("a response");
            serde_json::to_value(response).expect("serialize")["is"]["body"]
                .as_str()
                .expect("body")
                .to_string()
        }

        #[test]
        fn a_stub_key_sequencer_serves_the_queue_order_after_an_append() {
            // The recording changes the stub's content, so its `stub_key`: the queue's head must
            // be where a fresh cursor starts.
            let (imposter, recording) = sequenced(Arc::new(ByStubKey::default()));
            let mut served = vec![next_of(&imposter)];
            imposter.insert_or_append_proxy_stub(recording, "http://up", "proxyAlways", None, None);
            served.extend((0..5).map(|_| next_of(&imposter)));
            assert_eq!(served, ["A", "B", "C", "A", "U0", "B"]);
        }

        #[test]
        fn new_content_restarts_a_sequencer_cursor_over_an_order() {
            let (imposter, recording) =
                sequenced(Arc::new(crate::behaviors::LocalSequencer::default()));
            next_of(&imposter);
            imposter.insert_or_append_proxy_stub(recording, "http://up", "proxyAlways", None, None);
            assert_eq!([next_of(&imposter), next_of(&imposter)], ["B", "C"]);
            let replacement = serde_json::from_value(json!({
                "predicates": [{ "deepEquals": { "path": "/b" } }],
                "responses": [
                    { "is": { "body": "P" } },
                    { "is": { "body": "Q" } },
                    { "is": { "body": "R" } },
                    { "is": { "body": "S" } }
                ]
            }))
            .expect("replacement");
            assert!(imposter.replace_stub_by_id("s", replacement));
            // The sequencer stood at position 3 of the order; read as an index it would be `S`.
            assert_eq!([next_of(&imposter), next_of(&imposter)], ["P", "Q"]);
        }

        #[test]
        fn a_clamped_cursor_on_one_response_is_moved_to_it() {
            let state = state(&[("A", 1)]);
            state.cycler.set(2);
            let appended = append(&state, "U0");
            assert!(appended.cycle().is_none());
            let served: Vec<String> = (0..3).map(|_| served_body(&appended)).collect();
            assert_eq!(served, ["A", "U0", "A"]);
        }

        #[test]
        fn appending_several_to_an_empty_stub_does_not_panic() {
            let mut state = state(&[("A", 1)]);
            state.stub.responses.clear();
            state.cycler.set(2);
            let appended = state.with_appended(
                vec![response("X", 1), response("Y", 1)],
                state.cursor(),
                true,
            );
            let served: Vec<String> = (0..3).map(|_| served_body(&appended)).collect();
            assert_eq!(served, ["X", "Y", "X"]);
        }

        /// A literal model of Mountebank's `inMemoryImpostersRepository`: the display list and
        /// the rotating queue (`shift` + `push` per answer, `push` per append).
        struct Mountebank {
            responses: Vec<String>,
            queue: VecDeque<String>,
        }

        impl Mountebank {
            fn new(start: &[(String, u32)]) -> Self {
                Mountebank {
                    responses: start.iter().map(|(l, _)| l.clone()).collect(),
                    queue: start
                        .iter()
                        .flat_map(|(l, r)| std::iter::repeat_n(l.clone(), *r as usize))
                        .collect(),
                }
            }
            fn get(&mut self) -> String {
                let head = self.queue.pop_front().expect("non-empty queue");
                self.queue.push_back(head.clone());
                head
            }
            fn append(&mut self, label: &str) {
                self.responses.push(label.to_string());
                self.queue.push_back(label.to_string());
            }
        }

        proptest! {
            #[test]
            fn serve_and_display_order_match_mountebank(
                repeats in proptest::collection::vec(1u32..=3, 1..=4),
                trace in proptest::collection::vec(prop_oneof![3 => Just(false), 1 => Just(true)], 40),
            ) {
                let start: Vec<(String, u32)> = repeats
                    .iter()
                    .enumerate()
                    .map(|(i, r)| (format!("R{i}"), *r))
                    .collect();
                let borrowed: Vec<(&str, u32)> = start.iter().map(|(l, r)| (l.as_str(), *r)).collect();
                let mut mb = Mountebank::new(&start);
                let mut rift = state(&borrowed);
                for (step, is_append) in trace.into_iter().enumerate() {
                    if is_append {
                        let label = format!("U{step}");
                        mb.append(&label);
                        rift = append(&rift, &label);
                    } else {
                        prop_assert_eq!(served_body(&rift), mb.get(), "step {}", step);
                    }
                    prop_assert_eq!(labels(&rift), mb.responses.clone());
                }
            }
        }
    }
}
