//! ImposterManager - lifecycle management for multiple imposters.
//!
//! This module handles creating, deleting, and managing multiple imposters,
//! each running on its own port.

use super::core::Imposter;
use super::datadir_file;
use super::fault_io::{FaultCell, FaultIo, TcpFaultKind};
use super::handler::handle_imposter_request_decorated;
use super::reconcile::{
    ApplyReport, DeleteAllReport, DesiredImposter, EventContext, ImposterEvent,
    ImposterEventListener, Persistence, StubReconcile,
};
use super::types::{ImposterConfig, ImposterError, Stub, StubResponse, UnresolvableHost};
use crate::behaviors::ResponseSequencer;
use crate::extensions::decorate::ResponseDecorator;
use crate::extensions::exchange_inspector::ExchangeInspectorProvider;
use crate::extensions::flow_state::FlowStoreProvider;
use crate::extensions::metrics::PeerCheck;
use crate::extensions::no_match::NoMatchInterceptor;
use crate::imposter::journal::RequestJournal;
use crate::proxy::network::{
    AcceptBackoff, AcceptErrorClass, AcceptErrorEvent, AcceptErrorLog, PeerPolicy,
    classify_accept_error,
};
use crate::recording::ProxyRecordingStore;
use arc_swap::{ArcSwap, ArcSwapOption};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use tracing::{debug, error, info, warn};

/// Whether a loopback listener holds an auto-assign candidate: only the BSD family can hand one
/// out (see [`crate::proxy::network::loopback_listener_holds`]).
fn loopback_holder(port: u16) -> bool {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    return crate::proxy::network::loopback_listener_holds(port);
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    )))]
    {
        let _ = port;
        false
    }
}

/// One of the server's own listeners, holding a port no imposter may take (issue #1557). Named in
/// the refusal's log line, so an operator sees which listener a `400 already in use` was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerListener {
    /// The admin API.
    Admin,
    /// `GET /metrics`.
    Metrics,
    /// `--front-door`.
    FrontDoor,
    /// The intercept (HTTPS forward) proxy.
    Intercept,
}

impl std::fmt::Display for ServerListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Admin => "admin API",
            Self::Metrics => "metrics endpoint",
            Self::FrontDoor => "front door",
            Self::Intercept => "intercept proxy",
        })
    }
}

/// Ports one server listener reserved on an [`ImposterManager`] (issue #1557), released when this
/// is dropped. The listener's running handle holds it, so the port is the server's for exactly as
/// long as the listener is: the RAII form of [`ImposterManager::reserve_port`] and
/// [`ImposterManager::release_port`].
#[must_use = "dropping the reservation releases the port at once"]
pub struct PortReservation {
    manager: Arc<ImposterManager>,
    ports: Vec<u16>,
}

impl PortReservation {
    /// Reserve the ports of `addrs` for `by`. A listener bound on several addresses of one port
    /// (`--local-only`'s `127.0.0.1` and `[::1]`) reserves it once.
    pub fn new(
        manager: &Arc<ImposterManager>,
        addrs: &[std::net::SocketAddr],
        by: ServerListener,
    ) -> Self {
        let mut ports: Vec<u16> = addrs.iter().map(std::net::SocketAddr::port).collect();
        ports.sort_unstable();
        ports.dedup();
        for &port in &ports {
            manager.reserve_port(port, by);
        }
        Self {
            manager: Arc::clone(manager),
            ports,
        }
    }
}

impl std::fmt::Debug for PortReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortReservation")
            .field("ports", &self.ports)
            .finish_non_exhaustive()
    }
}

impl Drop for PortReservation {
    fn drop(&mut self) {
        for &port in &self.ports {
            self.manager.release_port(port);
        }
    }
}

/// Server-level TLS defaults for HTTPS imposters that don't carry their own cert/key (issue #206).
#[derive(Debug, Clone)]
pub struct TlsDefaults {
    /// Default cert/key PEM applied to any HTTPS imposter without inline `cert`/`key`.
    pub default_cert: Option<String>,
    pub default_key: Option<String>,
    /// Generate an in-memory self-signed cert when no other material is available (Mountebank
    /// zero-config default). When `false`, an HTTPS imposter without cert material is an error.
    pub allow_self_signed: bool,
}

impl Default for TlsDefaults {
    fn default() -> Self {
        Self {
            default_cert: None,
            default_key: None,
            allow_self_signed: true,
        }
    }
}

/// Serve one imposter connection (over plaintext or an already-handshaked TLS stream) until it
/// completes or the imposter is torn down. Auto-negotiates HTTP/1 and HTTP/2 (issue #295), except
/// for imposters that can fire a connection-level TCP fault, which are served HTTP/1-only. Shared
/// by the plain and HTTPS serve paths (issue #206). (Name kept for history.)
///
/// `http1_only` is computed once by the caller, before the TLS handshake even starts (issue
/// #1029), and reused here rather than recomputed: the same evaluation of the live stub set picks
/// both the ALPN offer (over TLS) and the hyper builder used below, so the two can never disagree
/// even though the underlying predicate (`Imposter::uses_tcp_faults`) can change between any two
/// connections without this function or the acceptor being rebuilt.
#[allow(clippy::too_many_arguments)]
async fn run_http1<I>(
    io: I,
    imposter: Arc<Imposter>,
    addr: std::net::SocketAddr,
    fault_cell: FaultCell,
    mut conn_shutdown_rx: broadcast::Receiver<()>,
    port: u16,
    decorator: Option<Arc<dyn ResponseDecorator>>,
    http_tuning: crate::proxy::network::HttpTuning,
    http1_only: bool,
) where
    // Tokio-side, not `hyper::rt`'s: the `auto` branch below must sniff the H2 preface on the raw
    // stream before `TokioIo::new` wraps it (issue #1030), so this takes the stream one layer
    // earlier than it used to and does its own `TokioIo::new` per branch.
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req| {
        let imposter = Arc::clone(&imposter);
        let fault_cell = fault_cell.clone();
        let decorator = decorator.clone();
        async move {
            let version = req.version();
            let response =
                handle_imposter_request_decorated(req, imposter, addr, port, decorator).await?;
            if let Some(kind) = response.extensions().get::<TcpFaultKind>().copied() {
                fault_cell.arm(kind);
            }
            super::fault_io::arm_linger_if_requested(&response, version, &fault_cell);
            Ok::<_, std::convert::Infallible>(response)
        }
    });

    // Both builders yield a Connection with the same drive/graceful-shutdown shape; only the
    // protocol negotiation differs.
    macro_rules! drive_conn {
        ($conn:expr) => {{
            let conn = $conn;
            tokio::pin!(conn);
            tokio::select! {
                res = conn.as_mut() => {
                    if let Err(e) = res {
                        debug!("Connection error on port {}: {}", port, e);
                    }
                }
                _ = conn_shutdown_rx.recv() => {
                    // Stop accepting new requests on this connection and close it once any in-flight
                    // request completes (issue #207).
                    conn.as_mut().graceful_shutdown();
                    if let Err(e) = conn.as_mut().await {
                        debug!("Connection error on port {} during shutdown: {}", port, e);
                    }
                }
            }
        }};
    }

    if http1_only {
        // `http1::Builder`'s own timer arms immediately (issue #1030 is specific to `auto`'s
        // preface sniff, which runs before any timer exists) — no sniffing needed here.
        let mut builder = hyper::server::conn::http1::Builder::new();
        builder
            // A timer is required for `header_read_timeout` to take effect (hyper panics on
            // serve_connection otherwise) — always paired with it below.
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(http_tuning.header_read_timeout)
            .max_buf_size(http_tuning.max_buf_size);
        drive_conn!(builder.serve_connection(TokioIo::new(io), service));
    } else {
        // Bound the HTTP/1-vs-HTTP/2 detection window itself (issue #1030): `auto::Builder`
        // sniffs the preface before either protocol's `Connection` — and its
        // `header_read_timeout` — exists, so a client that completes the handshake and then sends
        // nothing (or a partial H2 preface) would otherwise pin this task, the fd and, over TLS,
        // the `TlsStream` forever. Sniffed on the tokio-side stream, before `TokioIo::new`.
        //
        // Also raced against the shutdown signal: this runs before `drive_conn!` below, which is
        // otherwise the only place a connection observes shutdown, so without this an imposter
        // delete/rebind would have to wait out the full detection deadline for any connection
        // still stuck sniffing (issue #207's drain).
        let sniff = crate::proxy::preface::sniff_h2_preface(io, http_tuning.header_read_timeout);
        tokio::pin!(sniff);
        let sniffed = tokio::select! {
            result = &mut sniff => match result {
                Ok(sniffed) => sniffed,
                Err(e) => {
                    // Entirely client-controlled (a client choosing to stay silent), so this is
                    // not worth more than a debug log — a per-connection `warn!` here would be an
                    // unbounded log-volume lever for a hostile client (issue #718's rule).
                    crate::extensions::metrics::record_preface_failure("imposter", &e);
                    debug!("preface detection on port {}: {}", port, e);
                    return;
                }
            },
            _ = conn_shutdown_rx.recv() => return,
        };
        let mut builder =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
        builder
            .http1()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(http_tuning.header_read_timeout)
            .max_buf_size(http_tuning.max_buf_size);
        // Issue #1044: the h2 leg needs its own bound. `header_read_timeout` covers HTTP/1 and
        // the preface sniff covers the detection window (#1030), but neither reaches an h2
        // connection that has finished its handshake and then goes silent — hyper has no "first
        // request head" timer for h2 at all. Its keep-alive ping is the only knob that closes such
        // a peer, and it was simply never configured: `is_enabled()` is false unless an interval
        // is set, so no ping ever armed.
        //
        // The timer is not optional decoration. Setting an h2 timeout with no h2 timer makes
        // hyper panic (`Time::Empty`) on the first h2 connection, and every site here set a timer
        // on the h1 leg only.
        //
        // Reuses RIFT_HTTP_HEADER_TIMEOUT rather than adding a knob: it already means "how long a
        // client may hold a connection without producing a request head", which is exactly this.
        // Worst case for a silent peer is interval + timeout = 2x the value.
        builder
            .http2()
            .timer(hyper_util::rt::TokioTimer::new())
            .keep_alive_interval(http_tuning.header_read_timeout)
            .keep_alive_timeout(http_tuning.header_read_timeout);
        drive_conn!(builder.serve_connection(TokioIo::new(sniffed), service));
    }
}

/// A fixed slot table over the full `u16` port keyspace (issue #713), replacing
/// `RwLock<HashMap<u16, Arc<Imposter>>>` on the admin gateway's per-request port lookup.
/// A port is a `u16`, so a boxed slice of 65536 slots is a perfect hash: every port maps to
/// exactly one slot with no hashing, no resizing, and no contention between concurrent readers.
/// Reads (`get`/`contains`/`len`/`ports`/`imposters`) are wait-free — they only ever load an
/// `ArcSwapOption`, never take a lock. Only mutations (`try_claim`/`remove`) take `mutations`,
/// which serializes them against each other so the check-then-store inside `try_claim` stays
/// atomic (the same atomicity the old write-lock gave the create path's duplicate-port check).
pub(crate) struct PortTable {
    /// One `ArcSwapOption` per possible port. A `Box<[_]>` rather than `[_; 65536]`: `ArcSwapOption`
    /// isn't `Copy`, so a fixed-size array literal has no ergonomic init syntax, while a boxed
    /// slice built from an iterator indexes identically and is exactly as fixed-size at runtime.
    slots: Box<[ArcSwapOption<Imposter>]>,
    /// Live-entry count, maintained alongside the slots rather than derived by scanning them on
    /// every `len()` call (the admin `count()` hot path).
    count: std::sync::atomic::AtomicUsize,
    /// Serializes mutations (`try_claim`/`remove`) only; reads never take this. Mirrors the
    /// existing `stubs_write`-style mutation serialization elsewhere in this crate.
    mutations: parking_lot::Mutex<()>,
}

impl PortTable {
    fn new() -> Self {
        Self {
            slots: (0..=u16::MAX)
                .map(|_| ArcSwapOption::empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            count: std::sync::atomic::AtomicUsize::new(0),
            mutations: parking_lot::Mutex::new(()),
        }
    }

    /// Wait-free lookup: a single `ArcSwapOption` load, no lock.
    fn get(&self, port: u16) -> Option<Arc<Imposter>> {
        self.slots[port as usize].load_full()
    }

    /// Wait-free membership check.
    fn contains(&self, port: u16) -> bool {
        self.slots[port as usize].load().is_some()
    }

    /// Claim an empty slot, or refuse if it's already occupied. The `mutations` lock makes the
    /// check-then-store atomic across concurrent claimants — without it, two callers could both
    /// observe an empty slot and both store, silently losing one imposter (the same race the old
    /// write-lock's `contains_key` + `insert` pair closed).
    fn try_claim(&self, port: u16, imposter: Arc<Imposter>) -> Result<(), ()> {
        let _guard = self.mutations.lock();
        let slot = &self.slots[port as usize];
        if slot.load().is_some() {
            return Err(());
        }
        slot.store(Some(imposter));
        self.count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Vacate a slot only if it still holds `expected` (issue #1124): a delete that looked an imposter
    /// up and removed its file must not then tear down a different imposter that took the port
    /// meanwhile.
    fn remove_if_same(&self, port: u16, expected: &Arc<Imposter>) -> Option<Arc<Imposter>> {
        let _guard = self.mutations.lock();
        let slot = &self.slots[port as usize];
        if !slot
            .load()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            return None;
        }
        let previous = slot.swap(None);
        if previous.is_some() {
            self.count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        previous
    }

    /// Vacate a slot, returning what was there (if anything).
    fn remove(&self, port: u16) -> Option<Arc<Imposter>> {
        let _guard = self.mutations.lock();
        let previous = self.slots[port as usize].swap(None);
        if previous.is_some() {
            self.count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        previous
    }

    /// Live-entry count.
    fn len(&self) -> usize {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Occupied ports, ascending. A full 65536-slot scan, but only ever called from
    /// low-frequency admin paths (list/auto-assign/delete-all), never per-request.
    fn ports(&self) -> Vec<u16> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.load().is_some().then_some(i as u16))
            .collect()
    }

    /// Occupied imposters, ascending by port (see `ports`).
    fn imposters(&self) -> Vec<Arc<Imposter>> {
        self.slots
            .iter()
            .filter_map(|slot| slot.load_full())
            .collect()
    }
}

/// Manages the lifecycle of multiple imposters
pub struct ImposterManager {
    /// Active imposters by port, one wait-free slot per possible `u16` port (issue #713).
    imposters: PortTable,
    /// Global shutdown signal (for future graceful shutdown)
    shutdown_tx: broadcast::Sender<()>,
    /// Optional data directory for persistence write-through
    datadir: Option<Arc<PathBuf>>,
    /// Serializes datadir writes against a delete's unlink-then-remove (issue #1124), so a write
    /// cannot land between the two and bring back the file of an imposter being deleted. Taken only
    /// by admin mutations that persist, never by request handling.
    ///
    /// `Arc` so a write can hand an owned guard to its blocking thread: the lock is then held until
    /// the write's rename is done, even when the admin request that started it is dropped mid-write
    /// (issue #1158) — otherwise a second write could share its temp file, or a delete could run
    /// before its late rename.
    persist_lock: Arc<tokio::sync::Mutex<()>>,
    /// The last `_rift.conditional` load stamp, in seconds, of each port's deleted imposter (issue
    /// #1301). A client polls a URL, not an imposter, so the next imposter on that port continues
    /// the clock instead of stamping from the wall clock again — which would answer `304` for new
    /// content when the re-create lands in the second the old imposter last stamped. Covers every
    /// re-create: a reload that rewrites most stubs, `PUT /imposters`, delete + create. One entry
    /// per port ever deleted: at most 65,536, never evicted.
    load_floors: parking_lot::Mutex<std::collections::HashMap<u16, i64>>,
    /// TLS defaults for HTTPS imposters (issue #206)
    tls_defaults: TlsDefaults,
    /// Observer for config mutations (issue #316)
    event_listener: Option<Arc<dyn ImposterEventListener>>,
    /// Outgoing-response hook for imposter traffic (issue #318)
    response_decorator: Option<Arc<dyn ResponseDecorator>>,
    /// Embedder hook to supply a custom flow store per imposter (issue #312)
    flow_store_provider: Option<Arc<dyn FlowStoreProvider>>,
    /// Pluggable response-cursor backend (issue #313); None = embedded per-stub cycler.
    sequencer: Option<Arc<dyn ResponseSequencer>>,
    /// Pluggable recorded-request backend (issue #314); None = per-imposter LocalJournal.
    request_journal: Option<Arc<dyn RequestJournal>>,
    /// Byte budget for each imposter's own `LocalJournal` (issue #1441); `0` keeps the default. An
    /// atomic for the same reason `upstream_client` is an `ArcSwap`: the C-ABI sets it after the
    /// manager exists. Ignored when a shared journal is injected — that one keeps its own policy.
    journal_max_bytes: std::sync::atomic::AtomicUsize,
    /// Pluggable proxy-recording backend (issue #315); None = per-imposter LocalProxyStore.
    proxy_store: Option<Arc<dyn ProxyRecordingStore>>,
    /// Client every `proxy` stub dials upstream with (issue #974); empty = the process-wide default
    /// on the default trust policy.
    ///
    /// Behind an `ArcSwapOption` rather than a plain `Option` because the C-ABI builds its manager
    /// in `rift_start()`, before `rift_serve_admin` has parsed the options carrying the trust
    /// policy — a consuming builder cannot reach it by then. Imposters read this at creation, so a
    /// store before the first `create_imposter` is what every caller needs.
    upstream_client: ArcSwapOption<reqwest::Client>,
    /// Body limits and the in-flight budget every imposter, proxy read and admin body shares (issue
    /// #1439). An `ArcSwap` for the same reason as `upstream_client`: the C-ABI sets it after the
    /// manager exists. Imposters take the guard current at their creation.
    body_guard: ArcSwap<super::body_limits::BodyGuard>,
    /// Last-chance no-match hook (issue #819); None = no interceptor, unchanged fallthrough.
    no_match_interceptor: Option<Arc<dyn NoMatchInterceptor>>,
    /// Provider consulted per imposter for a synchronous exchange inspector (issue #966); None =
    /// no provider, so every imposter gets no inspector regardless of its config.
    exchange_inspector_provider: Option<Arc<dyn ExchangeInspectorProvider>>,
    /// Per-core accept runtimes (RFC-712, issue #745). When set, every imposter port binds one
    /// SO_REUSEPORT listener per runtime and each accept loop runs pinned to its runtime; the
    /// kernel spreads connections across them by 4-tuple hash. `None` (the default) keeps
    /// today's single listener on the ambient runtime.
    accept_runtimes: Option<Arc<Vec<tokio::runtime::Handle>>>,
    /// Register an imposter whose port could not be bound, instead of failing the apply
    /// (issue #143). `false` (the default) keeps the all-or-nothing behavior. See
    /// [`Self::with_serve_unbound`].
    serve_unbound: bool,
    /// The `host` an imposter binds when its config names none (issue #1517). See
    /// [`Self::with_default_host`].
    default_host: Option<Arc<str>>,
    /// Which clients every imposter's accept loops, and a front door over this manager, admit
    /// (issue #1523). See [`Self::with_peer_policy`].
    peer_policy: PeerPolicy,
    /// Upper bound on how long `delete` waits for a torn-down imposter's in-flight connections to
    /// drain before returning anyway with a warning (issue #596). Never configurable at runtime;
    /// overridable only in tests via [`Self::with_conn_drain`].
    conn_drain: Duration,
    /// Ports the server's own listeners hold (issue #1557). An imposter is refused on any of them,
    /// at bind time, because imposters bind with `SO_REUSEADDR`: on the BSD family that lets a
    /// wildcard bind sit beside a listener on a specific address, and the two then split the
    /// port's clients. Empty unless a listener registers itself with [`Self::reserve_port`].
    reserved: parking_lot::RwLock<std::collections::BTreeMap<u16, ServerListener>>,
    /// Broadcast of lifecycle + recorded-request events for the admin SSE stream (issue #461).
    /// Created unconditionally (a `broadcast::Sender` with no receivers is ~free); publishing is a
    /// no-op until a client subscribes.
    event_bus: Arc<super::events::AdminEventBus>,
}

/// Default bound for the post-delete connection drain (issue #596). Generous: normal graceful
/// shutdown of idle/short connections completes in milliseconds, so this only caps a genuinely
/// stalled in-flight request (e.g. a slow proxy upstream) rather than hanging the delete. A `wait`
/// behavior does not reach it: the teardown signal cuts the wait off (issue #1616).
const DEFAULT_CONN_DRAIN: Duration = Duration::from_secs(5);

impl ImposterManager {
    /// Create a new imposter manager without persistence
    pub fn new() -> Self {
        Self::with_datadir(None)
    }

    /// Create a new imposter manager with optional filesystem persistence
    pub fn with_datadir(datadir: Option<PathBuf>) -> Self {
        let (shutdown_tx, _) = broadcast::channel(16);
        Self {
            imposters: PortTable::new(),
            shutdown_tx,
            datadir: datadir.map(Arc::new),
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
            load_floors: parking_lot::Mutex::new(std::collections::HashMap::new()),
            tls_defaults: TlsDefaults::default(),
            event_listener: None,
            response_decorator: None,
            flow_store_provider: None,
            sequencer: None,
            request_journal: None,
            journal_max_bytes: std::sync::atomic::AtomicUsize::new(0),
            proxy_store: None,
            upstream_client: ArcSwapOption::empty(),
            body_guard: ArcSwap::from_pointee(super::body_limits::BodyGuard::default()),
            no_match_interceptor: None,
            exchange_inspector_provider: None,
            accept_runtimes: None,
            serve_unbound: false,
            default_host: None,
            peer_policy: PeerPolicy::Any,
            conn_drain: DEFAULT_CONN_DRAIN,
            reserved: parking_lot::RwLock::new(std::collections::BTreeMap::new()),
            event_bus: Arc::new(super::events::AdminEventBus::new()),
        }
    }

    /// Mark `port` as held by one of the server's own listeners (issue #1557), so no imposter can
    /// be created on it: an explicit port is refused with [`ImposterError::PortInUse`], the
    /// existing `400 already in use`, and auto-assign skips it.
    ///
    /// Called by each listener right after it binds, with the port it really got (`--port 0` is
    /// real), and undone with [`Self::release_port`] when it stops. An embedder that never calls
    /// it sees no change. Checked when an imposter binds, never when a config is decoded, so stored
    /// and replayed configs stay decodable.
    ///
    /// Under [`Self::with_serve_unbound`] an explicit-port imposter on a reserved port is
    /// registered without a listener, as a failed bind is, rather than refused.
    pub fn reserve_port(&self, port: u16, by: ServerListener) {
        self.reserved.write().insert(port, by);
    }

    /// Undo [`Self::reserve_port`]: imposters may take `port` again. A no-op for a port that is not
    /// reserved.
    pub fn release_port(&self, port: u16) {
        self.reserved.write().remove(&port);
    }

    /// Which server listener holds `port`, if one does (issue #1557).
    #[must_use]
    pub fn reserved_by(&self, port: u16) -> Option<ServerListener> {
        self.reserved.read().get(&port).copied()
    }

    /// Whether an imposter on `port` is accepting on its own listener (issue #1557). A server
    /// listener checks this before it binds, so a startup imposter that already took the port
    /// makes that bind fail instead of sharing the port with it on macOS. An imposter registered
    /// without a listener (serve-unbound, issue #143) holds no socket and does not count.
    #[must_use]
    pub fn imposter_listens_on(&self, port: u16) -> bool {
        self.imposters.get(port).is_some_and(|i| i.is_bound())
    }

    /// The refusal for an explicit port a server listener holds, logged with the holder's name.
    fn refuse_reserved(&self, port: u16) -> Result<(), ImposterError> {
        match self.reserved_by(port) {
            Some(holder) => {
                warn!(
                    port,
                    "refusing an imposter on port {port}: it is this server's {holder}"
                );
                Err(ImposterError::PortInUse(port))
            }
            None => Ok(()),
        }
    }

    /// The batch pre-check behind `apply_config` and `reload` (issue #1557): a reserved explicit
    /// port refuses the whole set before anything is torn down or created, as a duplicate port
    /// does. Skipped under [`Self::with_serve_unbound`], whose per-port create registers such an
    /// imposter without a listener instead.
    fn refuse_reserved_in_set<'a>(
        &self,
        configs: impl IntoIterator<Item = &'a ImposterConfig>,
    ) -> Result<(), ImposterError> {
        if self.serve_unbound {
            return Ok(());
        }
        configs
            .into_iter()
            .filter_map(ImposterConfig::explicit_port)
            .try_for_each(|port| self.refuse_reserved(port))
    }

    /// Keep serving an imposter whose port bind failed, instead of dropping it (issue #143).
    ///
    /// For a replicated deployment where every node applies the same committed config: a port can
    /// be taken by an unrelated process on *one* node, and that node must still serve the imposter
    /// in-process — the port map is what every in-process route resolves through, so an entry with
    /// no listener serves correctly; only traffic addressed to the port directly is lost, and that
    /// is the part the squatter has already taken.
    ///
    /// Scope, deliberately narrow:
    /// - **Apply path only.** [`Self::create_imposter`] stays all-or-nothing whatever this is set
    ///   to, because `Err` there means "the imposter does not exist" to every existing caller.
    /// - **Socket-level failures only** ([`ImposterError::BindError`]). A [`ImposterError::PortInUse`]
    ///   is a map-level conflict where an entry already exists, and an auto-assigned port minted
    ///   from a failed bind has no identity to register under.
    ///
    /// An unbound imposter reports itself via [`Imposter::is_bound`], is reported under
    /// [`ApplyReport::failed`] (never `created`) so the embedder's degraded-state tracking keeps
    /// working off the field it already reads, and is re-bound automatically by the next
    /// [`Self::apply_config`] once the port frees up.
    #[must_use]
    pub fn with_serve_unbound(mut self, serve_unbound: bool) -> Self {
        self.serve_unbound = serve_unbound;
        self
    }

    /// The `host` an imposter binds when its config names none: Mountebank's `--host`, which every
    /// host-less imposter inherits (`imposter.js:194-195`, issue #1517). An imposter's own `host`
    /// always wins.
    ///
    /// Never written into the config: it is a property of this server, not of the imposter, so
    /// `GET`, `?replayable`, `rift save`, the datadir and any replicated stored bytes are unchanged
    /// by it, and a datadir imposter follows the `--host` of whichever server loads it. `None` (the
    /// default) binds every interface, dual-stack `[::]` (#1514).
    ///
    /// `rift`'s own server sets this from `--host` only on the manager it builds itself; an
    /// embedder injecting its own manager decides for itself.
    #[must_use]
    pub fn with_default_host(mut self, host: Option<String>) -> Self {
        self.default_host = host.map(Arc::from);
        self
    }

    /// Refuse imposter clients the policy does not admit: Mountebank's `--localOnly`, which refuses
    /// every non-loopback client on every imposter (`util/ip.js:5-52`, issue #1523). The default,
    /// [`PeerPolicy::Any`], serves everyone.
    ///
    /// Checked once per accepted connection on every accept loop, before socket tuning, fault
    /// wrapping and TLS: a refused client's socket is dropped, so it gets no handshake and no
    /// response, and is never recorded or counted in `numberOfRequests`. A front door bound over
    /// this manager reads it too, through [`Self::peer_policy`]. Binds are unchanged: an imposter
    /// still listens where its `host` says, as Mountebank's do.
    ///
    /// `rift`'s own server sets this from `--local-only` only on the manager it builds itself; an
    /// injected manager keeps whatever its builder chose.
    #[must_use]
    pub fn with_peer_policy(mut self, policy: PeerPolicy) -> Self {
        self.peer_policy = policy;
        self
    }

    /// The policy set by [`Self::with_peer_policy`].
    #[must_use]
    pub fn peer_policy(&self) -> PeerPolicy {
        self.peer_policy
    }

    /// The host an imposter with this config binds: its own `host`, else [`Self::with_default_host`].
    fn bind_host_for<'a>(&'a self, config: &'a ImposterConfig) -> Option<&'a str> {
        config.host.as_deref().or(self.default_host.as_deref())
    }

    /// Fan imposter accept loops out across per-core worker runtimes (RFC-712, issue #745):
    /// one SO_REUSEPORT listener per runtime per port. An empty vec means "no fan-out" and
    /// keeps the default single-listener topology, so callers can pass a possibly-empty set
    /// without branching.
    #[must_use]
    pub fn with_accept_runtimes(mut self, runtimes: Vec<tokio::runtime::Handle>) -> Self {
        self.accept_runtimes = if runtimes.is_empty() {
            None
        } else {
            Some(Arc::new(runtimes))
        };
        self
    }

    /// Override the post-delete connection-drain bound (issue #596). Test-only: production always
    /// uses [`DEFAULT_CONN_DRAIN`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_conn_drain(mut self, drain: Duration) -> Self {
        self.conn_drain = drain;
        self
    }

    /// Set the server-level TLS defaults for HTTPS imposters (issue #206).
    #[must_use]
    pub fn with_tls_defaults(mut self, tls_defaults: TlsDefaults) -> Self {
        self.tls_defaults = tls_defaults;
        self
    }

    /// Register an observer for config mutations (issue #316). Events are delivered
    /// synchronously on the mutating call; the in-memory change has already been applied
    /// when the listener runs (persistence may still be pending or fail afterwards).
    #[must_use]
    pub fn with_event_listener(mut self, listener: Arc<dyn ImposterEventListener>) -> Self {
        self.event_listener = Some(listener);
        self
    }

    /// Register an outgoing-response decorator (issue #318). Invoked for every response an
    /// imposter serves (phase `DataPlane`, the imposter's port), with the annotations
    /// collected during the request. The admin server applies the same decorator with
    /// phase `Admin` via [`Self::response_decorator`].
    #[must_use]
    pub fn with_response_decorator(mut self, decorator: Arc<dyn ResponseDecorator>) -> Self {
        self.response_decorator = Some(decorator);
        self
    }

    /// The configured response decorator, if any — public so admin/embedder listeners can
    /// apply the same hook to their own responses.
    pub fn response_decorator(&self) -> Option<Arc<dyn ResponseDecorator>> {
        self.response_decorator.clone()
    }

    /// Register a provider that supplies a custom [`FlowStore`](crate::flow_state::FlowStore)
    /// per imposter (issue #312), consulted before the built-in in-memory store. It is the one
    /// seam for a store other than the in-memory one (issue #1463).
    /// A provider that always returns a shared store also fixes the construction-time NoOp
    /// caveat for scenario stubs added after an imposter is created.
    #[must_use]
    pub fn with_flow_store_provider(mut self, provider: Arc<dyn FlowStoreProvider>) -> Self {
        self.flow_store_provider = Some(provider);
        self
    }

    /// Register a pluggable response-cursor backend (issue #313), consulted for every
    /// response-cycling decision with a full `SequenceKey` and materialized repeats.
    /// Without one, imposters keep the embedded per-stub cycler (today's hot path,
    /// untouched). `reset_scope` fires on stub delete — direct or via an `apply_config`
    /// patch — (that stub's key), bulk stub replace, and imposter teardown (port-wide,
    /// the GC hook) — and on a stub replace by index (that stub's old key), which restarts the
    /// stub's cycle as Mountebank's `overwriteAtIndex` does (#1515). A replace by id deliberately
    /// does NOT reset: the slot survives, so slot-keyed cursors keep cycling; a `stub_key`-keyed
    /// backend whose key changed with the content keeps the old key's cursor until port teardown
    /// reclaims it.
    #[must_use]
    pub fn with_sequencer(mut self, sequencer: Arc<dyn ResponseSequencer>) -> Self {
        self.sequencer = Some(sequencer);
        self
    }

    /// Bound each imposter's recorded requests to `max_bytes` (issue #1441), oldest evicted first.
    /// Builder form of [`Self::set_journal_max_bytes`].
    #[must_use]
    pub fn with_journal_max_bytes(self, max_bytes: std::num::NonZeroUsize) -> Self {
        self.set_journal_max_bytes(max_bytes);
        self
    }

    /// Set the journal byte budget on an already-constructed manager (issue #1441). Imposters
    /// read it when they are created; those that already exist keep theirs. Has no effect on a
    /// journal injected with [`Self::with_request_journal`].
    pub fn set_journal_max_bytes(&self, max_bytes: std::num::NonZeroUsize) {
        self.journal_max_bytes
            .store(max_bytes.get(), std::sync::atomic::Ordering::SeqCst);
    }

    /// Register a pluggable recorded-request backend (issue #314), shared across imposters
    /// and keyed by port. Without one, each imposter keeps a private in-memory journal with
    /// a byte budget (see [`Self::with_journal_max_bytes`]) and clear-resets-count semantics. Imposter deletion clears the
    /// port's slice so stale entries never resurrect on a later imposter reusing the port.
    ///
    /// Caveat: deletion lets in-flight requests drain naturally, so a request completing
    /// after the port clear can write one late entry into the shared journal (the private
    /// default is immune — its storage dies with the imposter).
    #[must_use]
    pub fn with_request_journal(mut self, journal: Arc<dyn RequestJournal>) -> Self {
        self.request_journal = Some(journal);
        self
    }

    /// Register a pluggable proxy-recording backend (issue #315), shared across imposters and
    /// keyed by port. Without one, each imposter keeps a private in-memory
    /// [`LocalProxyStore`](crate::recording::LocalProxyStore) with the historical semantics
    /// (proxyOnce once-gate, caps) plus the release-on-error fix. Imposter deletion clears the
    /// port's saved responses so a later imposter reusing the port starts clean.
    ///
    /// Every claim names the mode of the proxy response being served
    /// ([`try_claim_as`](crate::recording::ProxyRecordingStore::try_claim_as), issue #1387), so a
    /// shared store serves any mix of modes; a store that keeps the default `try_claim_as`
    /// resolves the mode itself.
    #[must_use]
    pub fn with_proxy_store(mut self, store: Arc<dyn ProxyRecordingStore>) -> Self {
        self.proxy_store = Some(store);
        self
    }

    /// Use `client` for every `proxy` stub's upstream call (issue #974).
    ///
    /// This is how an operator-configured outbound TLS trust policy — an extra CA anchor, or
    /// skip-verify — reaches the imposters. Build it from
    /// [`OutboundTls`](crate::proxy::OutboundTls); the fallible part is realising the policy, which
    /// happens at the caller so a bad `--upstream-ca-file` fails at startup rather than on the
    /// first proxied request.
    ///
    /// The client must come from the same reqwest major this crate links (0.13): a `Client` from
    /// any other major is a different type and does not compile here.
    #[must_use]
    pub fn with_upstream_client(self, client: Arc<reqwest::Client>) -> Self {
        self.upstream_client.store(Some(client));
        self
    }

    /// Set the upstream client on an already-constructed manager (issue #974).
    ///
    /// The C-ABI needs this: its manager exists from `rift_start()`, while the trust policy arrives
    /// with `rift_serve_admin`'s options. Imposters read the client when they are created, so a
    /// store before the first `create_imposter` is equivalent to having built with it. Imposters
    /// that already exist keep the client they were created with.
    pub fn set_upstream_client(&self, client: Arc<reqwest::Client>) {
        // Imposters read the client when they are created, so any that already exist keep the
        // policy they were built with. An embedder that creates imposters before serving would
        // otherwise get the default policy back with no diagnostic — the exact `UnknownIssuer`
        // this issue exists to remove.
        let existing = self.count();
        if existing > 0 {
            warn!(
                existing,
                "outbound TLS policy set after {existing} imposter(s) were created; those keep \
                 the previous policy. Set it before creating imposters."
            );
        }
        self.upstream_client.store(Some(client));
    }

    /// Set the body limits and in-flight memory budget (issue #1439). Builder form of
    /// [`Self::set_body_limits`].
    #[must_use]
    pub fn with_body_limits(self, limits: super::body_limits::BodyLimits) -> Self {
        self.set_body_limits(limits);
        self
    }

    /// Set the body limits and in-flight memory budget on an already-constructed manager (issue
    /// #1439). Imposters read the guard when they are created, so set it before creating any;
    /// those that already exist keep the limits and budget they were created with.
    pub fn set_body_limits(&self, limits: super::body_limits::BodyLimits) {
        let existing = self.count();
        if existing > 0 {
            warn!(
                existing,
                "body limits set after {existing} imposter(s) were created; those keep the \
                 previous limits. Set them before creating imposters."
            );
        }
        self.body_guard
            .store(Arc::new(super::body_limits::BodyGuard::new(limits)));
    }

    /// The body guard the admin API and new imposters share (issue #1439).
    #[must_use]
    pub fn body_guard(&self) -> Arc<super::body_limits::BodyGuard> {
        self.body_guard.load_full()
    }

    /// Register a last-chance no-match interceptor (issue #819), consulted for every imposter
    /// request that matches no stub, before the defaultForward/defaultResponse/empty-200
    /// fallthrough — including when a default IS configured, since a `RetryMatch` rescue must
    /// outrank forwarding. See [`NoMatchInterceptor`] for the full contract.
    #[must_use]
    pub fn with_no_match_interceptor(mut self, interceptor: Arc<dyn NoMatchInterceptor>) -> Self {
        self.no_match_interceptor = Some(interceptor);
        self
    }

    /// Register a provider that supplies a synchronous exchange inspector per imposter (issue
    /// #966), consulted once with that imposter's config when it is created — mirroring
    /// [`Self::with_flow_store_provider`]. `None` from the provider means that imposter runs with
    /// no hooks at all; without a provider registered here, no imposter is ever consulted. See
    /// [`ExchangeInspector`](crate::extensions::exchange_inspector::ExchangeInspector) for what
    /// the hooks see and when they run.
    #[must_use]
    pub fn with_exchange_inspector_provider(
        mut self,
        provider: Arc<dyn ExchangeInspectorProvider>,
    ) -> Self {
        self.exchange_inspector_provider = Some(provider);
        self
    }

    fn emit(&self, event: ImposterEvent) {
        if let Some(listener) = &self.event_listener {
            // Built only when someone is listening — outside a request scope this is a cheap miss,
            // but there is no reason to pay even that when no listener is installed (issue #855).
            let ctx = EventContext {
                principal: crate::extensions::authz::current_principal(),
            };
            listener.on_event(&event, &ctx);
        }
        // Additionally fan the event out to the admin SSE bus (issue #461) — separate from the
        // single-slot embedder listener above, which callers may already hold.
        use super::events::ImposterAction;
        let (action, port) = match event {
            ImposterEvent::Created(p) => (ImposterAction::Created, Some(p)),
            ImposterEvent::Replaced(p) => (ImposterAction::Replaced, Some(p)),
            ImposterEvent::StubsChanged(p) => (ImposterAction::StubsChanged, Some(p)),
            ImposterEvent::EnabledChanged { port, .. } => {
                (ImposterAction::EnabledChanged, Some(port))
            }
            ImposterEvent::Deleted(p) => (ImposterAction::Deleted, Some(p)),
            ImposterEvent::AllDeleted => (ImposterAction::AllDeleted, None),
        };
        self.event_bus.publish_lifecycle(action, port);
    }

    /// The admin event bus (issue #461), for the SSE stream handler to subscribe to.
    #[must_use]
    pub fn event_bus(&self) -> &Arc<super::events::AdminEventBus> {
        &self.event_bus
    }

    /// Interpret `mutualAuth` / `rejectUnauthorized` / `ca` (issue #977) — the only place those
    /// three fields are read, so every downstream layer receives a decision rather than a config.
    ///
    /// **Every** ambiguous combination is rejected, not just some. An earlier draft warned on two
    /// of them for Mountebank parity and errored on the third, which is incoherent — and a `warn!`
    /// reaches the server log, never the `POST /imposters` response, so the author who wrote the
    /// setting is the one person who would not learn it does nothing. A security key that is
    /// accepted and quietly ignored is the defect this issue was filed over; reproducing it inside
    /// the fix would be worse than not fixing it.
    fn client_auth_for(
        config: &ImposterConfig,
    ) -> Result<crate::proxy::tls::ClientAuth, ImposterError> {
        use crate::proxy::tls::ClientAuth;

        let is_https = config.protocol.eq_ignore_ascii_case("https");
        if !config.mutual_auth {
            if config.reject_unauthorized || config.ca.is_some() {
                return Err(ImposterError::Tls(
                    "`rejectUnauthorized`/`ca` require `mutualAuth: true`: without it no client \
                     certificate is ever requested, so neither setting can take effect. Set \
                     `mutualAuth: true`, or drop them."
                        .to_string(),
                ));
            }
            return Ok(ClientAuth::Off);
        }

        if !is_https {
            return Err(ImposterError::Tls(format!(
                "`mutualAuth` requires `protocol: \"https\"`, got \"{}\" — a client certificate \
                 cannot be requested on a cleartext listener",
                config.protocol
            )));
        }

        if !config.reject_unauthorized {
            if config.ca.is_some() {
                return Err(ImposterError::Tls(
                    "`ca` requires `rejectUnauthorized: true`: without it the client certificate \
                     is required but its chain is never validated, so the supplied CA would be \
                     ignored. Set `rejectUnauthorized: true`, or drop `ca`."
                        .to_string(),
                ));
            }
            return Ok(ClientAuth::RequireAny);
        }

        let Some(ca_pems) = config.ca.as_deref().filter(|pems| !pems.is_empty()) else {
            return Err(ImposterError::Tls(
                "`rejectUnauthorized` requires `ca`: supply the PEM trust anchor(s) client \
                 certificates must chain to. (Mountebank falls back to the public CA bundle here; \
                 validating a client certificate against public roots would verify nothing.)"
                    .to_string(),
            ));
        };

        let mut ca = Vec::new();
        for pem in ca_pems {
            let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_bytes())
                .collect::<Result<_, _>>()
                .map_err(|e| ImposterError::Tls(format!("parsing a `ca` entry: {e}")))?;
            if certs.is_empty() {
                return Err(ImposterError::Tls(
                    "a `ca` entry contains no certificate: expected at least one \
                     -----BEGIN CERTIFICATE----- block"
                        .to_string(),
                ));
            }
            ca.extend(certs);
        }
        Ok(ClientAuth::Verify { ca })
    }

    /// Resolve the TLS acceptor pair for an HTTPS imposter by precedence: inline imposter cert/key
    /// → server default → self-signed fallback → error (never silent cleartext, issue #206).
    ///
    /// Returns [`crate::proxy::tls::TlsAcceptors`], not a single acceptor (issue #1029): whether an
    /// HTTPS imposter is served HTTP/1-only can change per-connection, live, through the admin API
    /// (a stub mutation) without this being re-resolved, so the ALPN offer cannot be decided once
    /// here — the caller picks between the two acceptors per connection instead.
    fn resolve_tls_acceptors(
        &self,
        config: &ImposterConfig,
    ) -> Result<crate::proxy::tls::TlsAcceptors, ImposterError> {
        // Derived BEFORE the cert/key precedence below, so the two creation errors fire whatever
        // the cert turns out to come from — inline, `TlsDefaults`, or the self-signed fallback.
        let client_auth = Self::client_auth_for(config)?;
        let from_pem = |cert: &str, key: &str| {
            crate::proxy::tls::tls_acceptors_from_pem(cert.as_bytes(), key.as_bytes(), &client_auth)
                .map_err(|e| ImposterError::Tls(e.to_string()))
        };
        match (&config.cert, &config.key) {
            (Some(cert), Some(key)) => return from_pem(cert, key),
            (None, None) => {}
            _ => {
                return Err(ImposterError::Tls(
                    "https imposter must provide both `cert` and `key`, or neither".to_string(),
                ));
            }
        }
        // Same both-or-neither rule for the server default: a half-configured default (e.g. only
        // --default-tls-cert) must error, not silently downgrade to a self-signed cert.
        match (
            &self.tls_defaults.default_cert,
            &self.tls_defaults.default_key,
        ) {
            (Some(cert), Some(key)) => return from_pem(cert, key),
            (None, None) => {}
            _ => {
                return Err(ImposterError::Tls(
                    "server default TLS must provide both cert and key, or neither".to_string(),
                ));
            }
        }
        if self.tls_defaults.allow_self_signed {
            return crate::proxy::tls::generate_self_signed_acceptors(&client_auth)
                .map_err(|e| ImposterError::Tls(e.to_string()));
        }
        Err(ImposterError::Tls(
            "protocol \"https\" requested but no cert/key provided and self-signed generation is disabled"
                .to_string(),
        ))
    }

    /// Create and start an imposter
    /// Returns the assigned port (which may have been auto-assigned if not specified)
    pub async fn create_imposter(&self, config: ImposterConfig) -> Result<u16, ImposterError> {
        self.create_imposter_as(config, Persistence::Datadir).await
    }

    /// [`create_imposter`](Self::create_imposter), choosing whether the imposter is written to the
    /// datadir (issue #1122). A startup loader creates what it read from `--configfile` or
    /// `--imposters` as [`Persistence::Ephemeral`]: its source is where it is re-read from.
    pub async fn create_imposter_as(
        &self,
        config: ImposterConfig,
        persistence: Persistence,
    ) -> Result<u16, ImposterError> {
        // `false`: the direct-create contract is all-or-nothing whatever `serve_unbound` says
        // (issue #143) — `Err` here means "the imposter does not exist" to every caller.
        let (port, _) = self
            .create_imposter_staged(config, false, persistence)
            .await?;
        self.emit(ImposterEvent::Created(port));
        Ok(port)
    }

    /// The create pipeline, shared by the direct and apply paths.
    ///
    /// When `allow_unbound` is set (issue #143) a socket-level bind failure on an *explicit* port
    /// does not abort the create: the imposter is constructed and claimed with no listener, so the
    /// port map — which every in-process route resolves through — still serves it. The returned
    /// `Option<ImposterError>` is that bind failure, for the caller to report; `None` means the
    /// imposter is bound and serving its own port.
    async fn create_imposter_staged(
        &self,
        mut config: ImposterConfig,
        allow_unbound: bool,
        persistence: Persistence,
    ) -> Result<(u16, Option<ImposterError>), ImposterError> {
        // Validate protocol first
        match config.protocol.as_str() {
            "http" | "https" => {}
            proto => return Err(ImposterError::InvalidProtocol(proto.to_string())),
        }

        // Client-auth settings are validated for EVERY protocol, not just https (issue #977).
        // `resolve_tls_acceptors` derives this again on the https branch; that is fine and
        // deliberate — `client_auth_for` is pure, and having it self-contained is what lets the
        // reload path (`rebind_imposter`) re-derive rather than cache a stale decision.
        // `resolve_tls_acceptors` below runs only on the https branch, so leaving the check there
        // would let `mutualAuth: true` on an http imposter through silently — the exact
        // accepted-and-ignored failure this issue exists to remove.
        Self::client_auth_for(&config)?;

        // For HTTPS, resolve the per-imposter TLS acceptor pair up front so a missing/invalid cert
        // fails loudly at creation rather than silently serving cleartext (issue #206).
        let tls_acceptors = if config.protocol.eq_ignore_ascii_case("https") {
            Some(self.resolve_tls_acceptors(&config)?)
        } else {
            None
        };

        // Owned (not borrowed from `config`): the per-core fan-out (#745) rebinds listeners
        // after `config` has moved into the imposter. Neither the server default host (#1517) nor
        // `None`, Mountebank's dual-stack `[::]` (#1514), is ever written back into `config.host`:
        // the config is what is printed, persisted and replicated, and Mountebank prints no host
        // either.
        let bind_host: Option<String> = self.bind_host_for(&config).map(str::to_owned);
        let explicit_port = config.explicit_port();

        // One fallible bind phase covering BOTH binds — the port probe and the fan-out listener
        // group — because either can fail with `BindError` and the degradation below has to cover
        // the fan-out topology too (that is the one clustered deployments actually run).
        // A port the server's own listeners hold (issue #1557) is, to a serve-unbound apply, a bind
        // that failed: on Linux that is exactly what it was before the reserved set existed.
        let reserved = explicit_port.and_then(|p| self.reserved_by(p).map(|holder| (p, holder)));
        let bound = match reserved {
            Some((p, holder)) if allow_unbound => Err(ImposterError::BindError(
                p,
                anyhow::anyhow!("port {p} is this server's {holder}"),
            )),
            _ => {
                self.bind_for_create(bind_host.as_deref(), explicit_port)
                    .await
            }
        };
        let (port, listeners, bind_failure) = match (bound, explicit_port) {
            (Ok((port, listeners)), _) => (port, Some(listeners), None),
            // Only a socket-level failure on an explicit port may degrade to serving unbound:
            // an auto-assigned port minted from a failed bind has no identity to register
            // under, and `PortInUse` means an entry already exists.
            (Err(e @ ImposterError::BindError(..)), Some(p)) if allow_unbound => {
                warn!(
                    port = p,
                    "could not bind {}:{p} ({e}); registering the imposter without a listener so \
                         in-process routes still serve it",
                    bind_host.as_deref().unwrap_or("[::]")
                );
                (p, None, Some(e))
            }
            (Err(e), _) => return Err(e),
        };

        config.port = Some(port);

        if let Some(bound) = listeners
            .as_ref()
            .and_then(|group| group.first())
            .and_then(|listener| listener.local_addr().ok())
        {
            info!("Imposter bound to {bound}");
        }
        // Create imposter
        // A private journal per imposter unless one is shared, sized by the manager's byte
        // budget when one is set (issue #1441).
        let journal = self.request_journal.clone().or_else(|| {
            std::num::NonZeroUsize::new(
                self.journal_max_bytes
                    .load(std::sync::atomic::Ordering::SeqCst),
            )
            .map(|max| {
                Arc::new(super::journal::LocalJournal::with_max_bytes(max))
                    as Arc<dyn RequestJournal>
            })
        });
        let mut imposter = Imposter::new_with_hooks_and_journal(
            config,
            self.flow_store_provider.as_ref(),
            self.sequencer.clone(),
            journal,
        )
        .map_err(|e| ImposterError::FlowStoreConfig(format!("{e:#}")))?;
        imposter.set_persistence(persistence);
        let floor = self.load_floors.lock().get(&port).copied();
        if let Some(floor) = floor {
            imposter.continue_load_clock(floor);
        }

        // Inject the shared proxy-recording store, if one is registered (issue #315);
        // otherwise the imposter keeps its private per-mode LocalProxyStore.
        if let Some(store) = &self.proxy_store {
            imposter.proxy_store = Arc::clone(store);
        }

        // Inject the operator-configured upstream client, if one is registered (issue #974);
        // otherwise the imposter keeps the process-wide default trust policy.
        if let Some(client) = self.upstream_client.load_full() {
            imposter.upstream_client = Some(client);
        }

        // One budget across every imposter, the proxy reads and the admin API (issue #1439).
        imposter.body_guard = self.body_guard.load_full();

        // Share the admin event bus so recorded requests fan out to the SSE stream (issue #461).
        imposter.event_bus = Some(Arc::clone(&self.event_bus));

        // Inject the shared no-match interceptor, if one is registered (issue #819).
        if let Some(interceptor) = &self.no_match_interceptor {
            imposter.no_match_interceptor = Some(Arc::clone(interceptor));
        }

        // Consult the exchange-inspector provider, if one is registered (issue #966), with this
        // imposter's own config — the provider decides per imposter, not per manager.
        if let Some(provider) = &self.exchange_inspector_provider {
            imposter.exchange_inspector = provider.provide(&imposter.config);
        }

        // Create shutdown channel for this imposter
        let (shutdown_tx, _) = broadcast::channel(1);
        imposter.shutdown_tx = Some(shutdown_tx.clone());

        let imposter = Arc::new(imposter);

        // Start serving — unless the bind failed and `allow_unbound` let it through, in which case
        // there is no listener to accept on and the imposter serves in-process only (issue #143).
        //
        // Stored unconditionally: this imposter is not in the map yet, so nothing else can reach it
        // and there is no rebind to race. The handles land here BEFORE `try_claim` below, which is
        // what makes `is_bound()` trustworthy for every imposter reachable through the map.
        if let Some(listeners) = listeners {
            *imposter.serve_handles.lock() = self.spawn_accept_loops(
                &imposter,
                listeners,
                &shutdown_tx,
                tls_acceptors.as_ref(),
                port,
            );
        }

        // Store imposter. `try_claim`'s internal mutex closes the same TOCTOU the old write lock
        // did between the earlier read-only check and the bind: with SO_REUSEADDR/REUSEPORT two
        // concurrent creates for the same explicit port can both bind, so the loser of the claim
        // race must stop the listener it just started rather than leave an orphan accepting on
        // the shared port.
        if self
            .imposters
            .try_claim(port, Arc::clone(&imposter))
            .is_err()
        {
            let _ = shutdown_tx.send(());
            return Err(ImposterError::PortInUse(port));
        }

        // Persist synchronously and surface the result: a create that can't be durably recorded
        // must fail loudly rather than return a success that vanishes on restart (issue #563).
        // On failure roll back the insert and stop the listener so `Err` keeps meaning "not
        // running" for every caller (create_for_apply / replace_imposter rely on that).
        if let Err(e) = self.persist_imposter_checked(&imposter).await {
            self.imposters.remove(port);
            // No file to clean up: a failed persist leaves `{port}.json` exactly as it was (issue
            // #1158). The unlink that used to be here, for a truncated file, also deleted a reload's
            // own input file, and could delete a concurrent create's freshly persisted one.
            let _ = shutdown_tx.send(());
            return Err(e);
        }

        Ok((port, bind_failure))
    }

    /// Resolve the port and bind its listener group.
    ///
    /// `explicit_port` is `Some` for a configured port and `None` to auto-assign. Extracted from
    /// `create_imposter_staged` so a later rebind (issue #143) binds through the identical path,
    /// and so the whole bind is one fallible unit the caller can choose to tolerate.
    async fn bind_for_create(
        &self,
        bind_host: Option<&str>,
        explicit_port: Option<u16>,
    ) -> Result<(u16, Vec<TcpListener>), ImposterError> {
        let (port, probe) = match explicit_port {
            Some(p) => {
                // Check if specified port is already in use
                if self.imposters.contains(p) {
                    return Err(ImposterError::PortInUse(p));
                }
                self.refuse_reserved(p)?;
                (p, Self::bind_probe(bind_host, p).await?)
            }
            None => self.bind_auto_port(bind_host).await?,
        };
        // The group binds the address the probe actually took, so a name is resolved only once.
        let addr = probe
            .local_addr()
            .map_err(|e| ImposterError::BindError(port, anyhow::Error::new(e)))?;
        let listeners = self.bind_listener_group(addr, Some(probe))?;
        Ok((port, listeners))
    }

    /// The listener group for one port under the current accept topology.
    ///
    /// Under per-core fan-out (RFC-712, issue #745) the port gets one SO_REUSEPORT listener per
    /// worker runtime; every loop shares one imposter, one connection tracker, one shutdown
    /// broadcast and one backpressure semaphore, so delete/drain (#596) and the global connection
    /// cap (#716) are topology-independent. All listeners are bound BEFORE any loop spawns: a bind
    /// failure just drops the already-bound listeners — a port is never half-bound.
    ///
    /// `probe` is the listener the caller already bound to learn the port (auto-assign) or to check
    /// it (explicit); `None` when rebinding an existing imposter, which has no probe to hand over.
    fn bind_listener_group(
        &self,
        addr: std::net::SocketAddr,
        probe: Option<TcpListener>,
    ) -> Result<Vec<TcpListener>, ImposterError> {
        let port = addr.port();
        match &self.accept_runtimes {
            None => match probe {
                Some(listener) => Ok(vec![listener]),
                None => Ok(vec![
                    crate::proxy::network::create_reusable_listener(addr)
                        .map_err(|e| ImposterError::BindError(port, anyhow::Error::new(e)))?,
                ]),
            },
            Some(runtimes) => {
                // The probe is a reusable listener (explicit or auto-assigned, issue #1501), so it
                // joins the SO_REUSEPORT group as its first member: the port is never released
                // between learning it and serving it.
                let mut group = Vec::with_capacity(runtimes.len());
                group.extend(probe);
                while group.len() < runtimes.len() {
                    group.push(
                        crate::proxy::network::create_reusable_listener(addr)
                            .map_err(|e| ImposterError::BindError(port, anyhow::Error::new(e)))?,
                    );
                }
                Ok(group)
            }
        }
    }

    /// One reusable listener on `port` (SO_REUSEADDR/REUSEPORT, so a hot-reload (#197) can re-bind
    /// the port right after the previous listener is torn down): at `host`, or with none where
    /// Mountebank's Node listens by default, `[::]` dual-stack, else `0.0.0.0` (#1514).
    async fn bind_probe(host: Option<&str>, port: u16) -> Result<TcpListener, ImposterError> {
        let bound = match host {
            Some(host) => crate::proxy::network::create_reusable_listener(
                Self::resolve_bind_addr(host, port).await?,
            ),
            None => crate::proxy::network::bind_unspecified(
                port,
                crate::proxy::network::create_reusable_listener,
            ),
        };
        bound.map_err(|e| ImposterError::BindError(port, anyhow::Error::new(e)))
    }

    /// An IP literal in either IPv6 spelling binds as written; anything else is resolved as a name,
    /// as Mountebank's `listen(port, host)` would (issue #1137). The lookup runs off the async
    /// worker (`lookup_host`), since a slow resolver must not stall other imposters' accept loops.
    async fn resolve_bind_addr(
        bind_host: &str,
        port: u16,
    ) -> Result<std::net::SocketAddr, ImposterError> {
        if let Some(addr) = crate::proxy::bind_addr(bind_host, port) {
            return Ok(addr);
        }
        tokio::net::lookup_host((bind_host, port))
            .await
            .map_err(|e| {
                ImposterError::BindError(port, anyhow::Error::new(e).context(UnresolvableHost))
            })?
            .next()
            .ok_or_else(|| {
                ImposterError::BindError(
                    port,
                    anyhow::anyhow!("no socket address").context(UnresolvableHost),
                )
            })
    }

    /// Spawn one accept loop per listener and **return** their handles.
    ///
    /// The caller installs them into [`Imposter::serve_handles`], because the two callers need
    /// different install rules: the create path owns an imposter nothing else can reach yet and
    /// stores them unconditionally, while [`Self::rebind_imposter`] is mutating an imposter that is
    /// already in the map and must not clobber a concurrent rebind's handles (issue #143). Handing
    /// the vector back rather than installing it here is what lets that decision live with the
    /// caller that knows the rule.
    #[must_use]
    fn spawn_accept_loops(
        &self,
        imposter: &Arc<Imposter>,
        listeners: Vec<TcpListener>,
        shutdown_tx: &broadcast::Sender<()>,
        tls_acceptors: Option<&crate::proxy::tls::TlsAcceptors>,
        port: u16,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let response_decorator = self.response_decorator.clone();
        // Read socket/HTTP tuning once per imposter, not per accepted connection.
        let socket_tuning = crate::proxy::network::SocketTuning::from_env();
        let http_tuning = crate::proxy::network::HttpTuning::from_env();
        let peer_policy = self.peer_policy;
        // `None` (the default) preserves today's behavior exactly: no semaphore, no permit,
        // accept as fast as the kernel hands connections over. Created once per imposter and
        // shared by every listener so `RIFT_MAX_CONNECTIONS` keeps its global meaning under
        // fan-out (#716, #745). INVARIANT: construction must stay OUTSIDE the per-listener
        // spawn loop below — moving it inside would silently multiply the global cap by N,
        // and no test pins this (env-injected cap tests are racy); this comment is the guard.
        let connection_semaphore = http_tuning
            .max_connections
            .map(|n| Arc::new(tokio::sync::Semaphore::new(n)));

        let mut serve_handles = Vec::with_capacity(listeners.len());
        for (index, listener) in listeners.into_iter().enumerate() {
            // Resolved once per loop: `inc()` on the returned Counter is a bare atomic add,
            // so the accept path pays no label lookup or allocation (issue #746).
            let accept_counter = crate::extensions::metrics::ACCEPTED_CONNECTIONS_TOTAL
                .with_label_values(&[index.to_string().as_str()]);
            let peer_check = PeerCheck::new(peer_policy, "imposter", port);
            let loop_future = Self::run_accept_loop(
                listener,
                Arc::clone(imposter),
                shutdown_tx.clone(),
                shutdown_tx.subscribe(),
                connection_semaphore.clone(),
                tls_acceptors.cloned(),
                response_decorator.clone(),
                socket_tuning,
                http_tuning,
                port,
                accept_counter,
                peer_check,
            );
            serve_handles.push(match &self.accept_runtimes {
                Some(runtimes) => runtimes[index].spawn(loop_future),
                None => tokio::spawn(loop_future),
            });
        }
        serve_handles
    }

    /// Re-attempt the listener bind for an imposter that is registered but unbound (issue #143).
    ///
    /// Without this an unbound imposter would never heal: reconcile sees an entry with an unchanged
    /// config and does nothing, where before this feature the *absence* of an entry made every
    /// apply retry the bind. On success the imposter starts accepting on its own port again, using
    /// the shutdown channel it was constructed with so `delete` still stops it through one signal.
    async fn rebind_imposter(&self, imposter: &Arc<Imposter>) -> Result<(), ImposterError> {
        let port = imposter
            .config
            .port
            .ok_or_else(|| ImposterError::BindError(0, anyhow::anyhow!("imposter has no port")))?;
        let bind_host = self.bind_host_for(&imposter.config);
        // Re-resolved rather than cached: an https imposter must not silently start serving
        // cleartext on rebind if its cert has since become unreadable.
        let tls_acceptors = if imposter.config.protocol.eq_ignore_ascii_case("https") {
            Some(self.resolve_tls_acceptors(&imposter.config)?)
        } else {
            None
        };
        let Some(shutdown_tx) = imposter.shutdown_tx.clone() else {
            return Err(ImposterError::BindError(
                port,
                anyhow::anyhow!("imposter has no shutdown channel to drive an accept loop"),
            ));
        };
        // Still a server listener's port (issue #1557): stay unbound, as a failed bind does.
        if let Some(holder) = self.reserved_by(port) {
            return Err(ImposterError::BindError(
                port,
                anyhow::anyhow!("port {port} is this server's {holder}"),
            ));
        }

        // The group binds the address the probe took, so a host-less imposter that fell back to
        // IPv4 rebinds IPv4 throughout.
        let probe = Self::bind_probe(bind_host, port).await?;
        let addr = probe
            .local_addr()
            .map_err(|e| ImposterError::BindError(port, anyhow::Error::new(e)))?;
        let listeners = self.bind_listener_group(addr, Some(probe))?;
        let handles = self.spawn_accept_loops(
            imposter,
            listeners,
            &shutdown_tx,
            tls_acceptors.as_ref(),
            port,
        );

        // Install only if nobody else already did. Unlike the create path, this imposter is
        // already in the map, and two `apply_config` calls can run against the same engine
        // concurrently — the raft apply loop and the startup reconcile poll drive it through
        // separate state-machine handles with no lock between them, which is exactly the window a
        // node restarting into a squatted port lives in. Both would see `!is_bound()`, both would
        // bind (SO_REUSEPORT permits it), and an unconditional store would leave the loser's accept
        // loops running untracked: dropping a `JoinHandle` detaches rather than aborts, so `delete`
        // would stop awaiting them and its issue-#596 guarantee — the bind is released before
        // delete returns — would silently break, and the shared connection cap would double.
        //
        // No `await` between the check and the store, so the lock covers the whole decision.
        let mut installed = imposter.serve_handles.lock();
        if installed.is_empty() {
            *installed = handles;
            drop(installed);
            info!("Imposter rebound to {addr}");
        } else {
            // Lost the race: abort our loops so they stop accepting and drop their listeners,
            // rather than leaving a second untracked generation on the shared port.
            drop(installed);
            for handle in handles {
                handle.abort();
            }
        }
        Ok(())
    }

    /// One listener's accept loop, extracted verbatim from `create_imposter_staged` so the
    /// per-core fan-out (issue #745) can run N of them. Parameters are the loop's former
    /// captures — `imposter_clone` keeps its capture-era name precisely so the body is a
    /// verbatim move with zero semantic drift.
    #[allow(clippy::too_many_arguments)]
    async fn run_accept_loop(
        listener: TcpListener,
        imposter_clone: Arc<Imposter>,
        conn_shutdown_tx: broadcast::Sender<()>,
        mut shutdown_rx: broadcast::Receiver<()>,
        connection_semaphore: Option<Arc<tokio::sync::Semaphore>>,
        tls_acceptors: Option<crate::proxy::tls::TlsAcceptors>,
        response_decorator: Option<Arc<dyn ResponseDecorator>>,
        socket_tuning: crate::proxy::network::SocketTuning,
        http_tuning: crate::proxy::network::HttpTuning,
        port: u16,
        accept_counter: prometheus::Counter,
        mut peer_check: PeerCheck,
    ) {
        // Per-loop, unshared: N loops on one port degrade independently, and cross-loop
        // coordination would add a shared cache line to the accept path for no behavioral gain
        // (issue #750).
        let mut backoff = AcceptBackoff::new();
        let mut error_log = AcceptErrorLog::default();
        // Clears its contribution on every exit path, including a cancel break or a panic (#838).
        let mut outage = crate::extensions::AcceptOutageGuard::new("imposter");
        // Resolved once per loop, not per error (#840).
        let accept_errors = crate::extensions::AcceptErrorCounters::new("imposter");
        crate::extensions::metrics::materialize_preface_failure_counters("imposter");
        loop {
            // Acquire a permit *before* accepting so a cap holds connections back in the
            // listener backlog/kernel SYN queue rather than accepting them and then failing
            // downstream (issue #716). Raced against shutdown (not just `select!`-dropped)
            // because `delete_imposter_inner` awaits this task with no timeout — without the
            // race, a cap saturated by long-lived connections would make delete hang until one
            // freed up, instead of tearing down promptly.
            let permit = match &connection_semaphore {
                Some(sem) => {
                    let acquire = sem.clone().acquire_owned();
                    tokio::select! {
                        acquired = acquire => match acquired {
                            Ok(permit) => Some(permit),
                            // Only closes on Drop, which never happens here — but if it ever
                            // does, stop accepting rather than panic.
                            Err(_) => break,
                        },
                        _ = shutdown_rx.recv() => {
                            info!("Imposter on port {} shutting down", port);
                            break;
                        }
                    }
                }
                None => None,
            };
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            accept_counter.inc();
                            // Recovery: only the transition out of a systemic-error state logs
                            // and resets the backoff, so a healthy accept path pays one branch.
                            if let Some(suppressed) = error_log.on_success() {
                                outage.exit();
                                info!(
                                    port,
                                    suppressed,
                                    "accept loop on port {port} recovered after {suppressed} \
                                     suppressed error(s)"
                                );
                                backoff.reset();
                            }
                            // `--local-only` (issue #1523): refused before tuning, FaultIo and
                            // TLS. Dropping the stream is Mountebank's `socket.destroy()`; the
                            // permit drops with this iteration, so a refusal holds no slot.
                            if !peer_check.admits(addr) {
                                drop(stream);
                                continue;
                            }
                            crate::proxy::network::apply_stream_tuning(&stream, &socket_tuning);
                            let imposter = Arc::clone(&imposter_clone);
                            // Each connection watches the shutdown signal so existing
                            // keep-alive connections are gracefully closed on delete,
                            // not just new connections (issue #207).
                            let conn_shutdown_rx = conn_shutdown_tx.subscribe();
                            // One evaluation drives both the ALPN offer (which acceptor is picked
                            // below) and the hyper builder inside `run_http1` (issue #1029) — this
                            // closes the TOCTOU a re-evaluation after the handshake would leave: the
                            // live stub set (`uses_tcp_faults`) can change between any two
                            // connections without the acceptor pair being rebuilt.
                            let http1_only =
                                imposter.uses_tcp_faults() || crate::util::http2_disabled();
                            // Per-imposter TLS acceptors are cheap to clone (Arc-backed).
                            let tls_acceptor = tls_acceptors
                                .as_ref()
                                .map(|acceptors| acceptors.for_connection(http1_only).clone());
                            let decorator = response_decorator.clone();
                            // Track the connection task so `delete` can drain it (issue #596).
                            imposter_clone.conn_tracker.spawn(async move {
                                // Held for the connection's lifetime; released back to the
                                // semaphore when this task ends (issue #716).
                                let _permit = permit;
                                // Per-connection slot for a real transport fault (issue #239);
                                // armed by the handler, applied by FaultIo on the response write.
                                let fault_cell = FaultCell::new();
                                // FaultIo sits beneath TLS so #239 connection faults still break
                                // an HTTPS connection.
                                let faulted = FaultIo::new(stream, fault_cell.clone());
                                match tls_acceptor {
                                    // Bound the TLS handshake so a stalled/half-open client
                                    // can't pin the connection task indefinitely.
                                    Some(acceptor) => match tokio::time::timeout(
                                        std::time::Duration::from_secs(10),
                                        acceptor.accept(faulted),
                                    )
                                    .await
                                    {
                                        Ok(Ok(tls)) => {
                                            run_http1(
                                                tls,
                                                imposter,
                                                addr,
                                                fault_cell,
                                                conn_shutdown_rx,
                                                port,
                                                decorator,
                                                http_tuning,
                                                http1_only,
                                            )
                                            .await
                                        }
                                        Ok(Err(e)) => {
                                            debug!("TLS handshake failed on port {}: {}", port, e)
                                        }
                                        Err(_) => {
                                            debug!("TLS handshake timed out on port {}", port)
                                        }
                                    },
                                    None => {
                                        run_http1(
                                            faulted,
                                            imposter,
                                            addr,
                                            fault_cell,
                                            conn_shutdown_rx,
                                            port,
                                            decorator,
                                            http_tuning,
                                            http1_only,
                                        )
                                        .await
                                    }
                                }
                            });
                        }
                        Err(e) => match classify_accept_error(&e) {
                            // Expected under load — retry immediately, no backoff, no error spam.
                            AcceptErrorClass::Transient => {
                                accept_errors.record_transient();
                                debug!("transient accept error on port {port}: {e}");
                            }
                            // Systemic (fd exhaustion / unknown): log on entry and periodically
                            // while it persists, then back off.
                            AcceptErrorClass::Systemic => {
                                accept_errors.record_systemic();
                                match error_log.on_error() {
                                    Some(AcceptErrorEvent::Onset) => {
                                        outage.enter();
                                        error!(
                                            port,
                                            "accept error on port {port}: {e}; backing off \
                                             (further errors suppressed until recovery)"
                                        );
                                    }
                                    Some(AcceptErrorEvent::StillDown { suppressed }) => {
                                        error!(
                                            port,
                                            suppressed,
                                            "port {port} still failing to accept after \
                                             {suppressed} suppressed error(s): {e}"
                                        );
                                    }
                                    None => {}
                                }
                                // Raced against shutdown exactly like the permit acquire above:
                                // `delete_imposter_inner` awaits this task, so an un-raced sleep
                                // would stall delete/drain for up to the 1s cap × N loops (#745).
                                let delay = backoff.next_delay();
                                tokio::select! {
                                    _ = tokio::time::sleep(delay) => {}
                                    _ = shutdown_rx.recv() => {
                                        info!("Imposter on port {} shutting down", port);
                                        break;
                                    }
                                }
                            }
                        },
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("Imposter on port {} shutting down", port);
                    break;
                }
            }
        }
    }

    /// Bind an auto-assigned port. The kernel picks it, as Mountebank's `listen(0)` does, and the
    /// listener that learned the port is the one that serves it (issue #1501). The old scan from
    /// 49152 probed each port with `SO_REUSEADDR`, which macOS lets "succeed" on a port another
    /// process holds on loopback, and that process then kept every `127.0.0.1` connection.
    ///
    /// A candidate is refused when an imposter still owns it without a listener (serve-unbound,
    /// issue #143) or, for the IPv4 wildcard, when another socket listens on it on a family or
    /// address a client may reach first: IPv6-only (`localhost` resolving `::1`), or `127.0.0.1` on
    /// the BSD family, whose port-0 search does not rule that out. A refused candidate stays bound
    /// until a port is chosen, so the kernel cannot hand it out again.
    ///
    /// A host-less imposter binds `[::]` dual-stack (#1514), where no IPv6-only listener can sit
    /// beside it; a `127.0.0.1` listener still can on the BSD family, so that check applies to
    /// either wildcard.
    async fn bind_auto_port(
        &self,
        host: Option<&str>,
    ) -> Result<(u16, TcpListener), ImposterError> {
        const ATTEMPTS: usize = 16;
        let mut refused = Vec::new();
        for _ in 0..ATTEMPTS {
            let listener = Self::bind_probe(host, 0).await?;
            let bound = listener
                .local_addr()
                .map_err(|e| ImposterError::BindError(0, anyhow::Error::new(e)))?;
            let port = bound.port();
            let v4_wildcard = bound.ip() == std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
            if self.imposters.contains(port)
                || self.reserved_by(port).is_some()
                || (v4_wildcard && crate::proxy::network::ipv6_only_listener_holds(port))
                || (bound.ip().is_unspecified() && loopback_holder(port))
            {
                refused.push(listener);
                continue;
            }
            return Ok((port, listener));
        }
        Err(ImposterError::BindError(
            0,
            anyhow::anyhow!("no free port after {ATTEMPTS} attempts"),
        ))
    }

    /// Delete an imposter, returning its config with the stubs it served at delete time (#1229).
    pub async fn delete_imposter(&self, port: u16) -> Result<ImposterConfig, ImposterError> {
        Ok(self.delete_imposter_returning(port).await?.current_config())
    }

    /// Delete an imposter, returning the removed [`Imposter`] itself (issue #1399). It is already torn
    /// down, so its request counters and recordings are final: the state `DELETE /imposters/:port`
    /// reports. Emits `ImposterEvent::Deleted` exactly once, like [`Self::delete_imposter`].
    pub async fn delete_imposter_returning(
        &self,
        port: u16,
    ) -> Result<Arc<Imposter>, ImposterError> {
        let imposter = self.delete_imposter_inner(port, true).await?;
        self.emit(ImposterEvent::Deleted(port));
        Ok(imposter)
    }

    /// Delete without emitting an event, so a wholesale replace in `apply_config` reports one
    /// `Replaced` instead of Deleted+Created. `unlink_file` removes the imposter's `{port}.json`.
    /// Keep `imposter`'s load clock for the next imposter on `port` (issue #1301). Recorded before
    /// the imposter leaves the map, so a same-port create that wins the freed slot already sees it;
    /// a delete that then fails leaves a floor behind, which is harmless — floors only rise.
    fn record_load_floor(&self, port: u16, imposter: &Imposter) {
        let floor = imposter.load_clock.floor_secs();
        self.load_floors
            .lock()
            .entry(port)
            .and_modify(|f| *f = (*f).max(floor))
            .or_insert(floor);
    }

    async fn delete_imposter_inner(
        &self,
        port: u16,
        unlink_file: bool,
    ) -> Result<Arc<Imposter>, ImposterError> {
        // The file goes first (issue #1124): a delete that cannot remove it fails with the imposter
        // still registered and serving, so `Err` keeps meaning "not deleted". Removing the imposter
        // first would leave nothing to fall back to, and the file would bring it back on restart.
        let imposter = if unlink_file {
            // Held until the imposter is out of the map, so no write can recreate its file between
            // the unlink and the removal.
            let _persist = self.persist_lock.lock().await;
            let imposter = self.get_imposter(port)?;
            self.remove_persisted_imposter(&imposter).await?;
            self.record_load_floor(port, &imposter);
            self.imposters
                .remove_if_same(port, &imposter)
                .ok_or(ImposterError::NotFound(port))?
        } else {
            self.record_load_floor(port, &*self.get_imposter(port)?);
            self.imposters
                .remove(port)
                .ok_or(ImposterError::NotFound(port))?
        };

        // Signal the accept loop and every live connection to stop (issue #207), then **await** the
        // teardown so this delete does not return until the old generation can no longer serve
        // (issue #596): otherwise a same-port re-create races a still-bound listener / still-open
        // keep-alive connection and can be answered by the previous generation's state.
        if let Some(ref tx) = imposter.shutdown_tx {
            let _ = tx.send(());
        }
        // Await the accept loop: its `select!` breaks on the signal and drops the listener, so the
        // bind is released by the time this resolves. A `JoinError` (loop task panicked) is
        // irrelevant here — we only need it stopped — but is surfaced as a warning rather than swallowed.
        let serve_handles: Vec<_> = std::mem::take(&mut *imposter.serve_handles.lock());
        for handle in serve_handles {
            if let Err(e) = handle.await
                && !e.is_cancelled()
            {
                warn!("imposter on port {port}: accept loop ended abnormally during delete: {e}");
            }
        }
        // Drain in-flight connections within a bound. Each reacts to the signal above with a hyper
        // graceful shutdown (finish the in-flight response, then close), and a request sleeping in
        // a `wait` is cut off by the same signal with its connection closed (issue #1616), so this
        // is normally sub-millisecond; a genuinely stalled request (e.g. a slow proxy upstream) is
        // capped rather than hanging the delete.
        imposter.conn_tracker.close();
        if tokio::time::timeout(self.conn_drain, imposter.conn_tracker.wait())
            .await
            .is_err()
        {
            warn!(
                "imposter on port {port}: connection drain exceeded {:?}; returning with {} connection(s) still closing",
                self.conn_drain,
                imposter.conn_tracker.len()
            );
        }

        // Clear JavaScript inject state for this imposter
        #[cfg(feature = "javascript")]
        crate::scripting::clear_imposter_state(port);

        if let Some(sequencer) = &self.sequencer {
            sequencer.reset_scope(port, None);
        }
        if let Some(journal) = &self.request_journal {
            // GC clear is best-effort: a failed backend clear must not fail the delete, but it
            // is logged rather than dropped (issue #330).
            if let Err(e) = journal.clear(port) {
                warn!("failed to clear request journal for deleted imposter on port {port}: {e:#}");
            }
        }
        // Reclaim the shared proxy store's port slice so a later imposter reusing the port
        // doesn't inherit stale recordings (issue #315). The private default dies with the
        // imposter, so it needs no explicit clear.
        if let Some(store) = &self.proxy_store {
            store.clear(port);
        }

        info!("Imposter on port {} deleted", port);
        // Handed back whole, so callers read the config as it was at delete time (issue #1229:
        // `DELETE /imposters` is Mountebank's save-before-reset export) and the final counters (#1399).
        Ok(imposter)
    }

    /// Get an imposter by port
    pub fn get_imposter(&self, port: u16) -> Result<Arc<Imposter>, ImposterError> {
        self.imposters
            .get(port)
            .ok_or(ImposterError::NotFound(port))
    }

    /// List all imposters, ascending by port (issue #713) — was arbitrary `HashMap` order.
    pub fn list_imposters(&self) -> Vec<Arc<Imposter>> {
        self.imposters.imposters()
    }

    /// Delete all imposters. Emits a single `AllDeleted` event rather than one `Deleted` per port,
    /// unless an imposter could not be deleted (its datadir file could not be removed, issue #1124):
    /// then each deleted port gets its own `Deleted`, because the others are still serving.
    pub async fn delete_all(&self) -> DeleteAllReport {
        let mut report = DeleteAllReport::default();
        for port in self.imposters.ports() {
            match self.delete_imposter_inner(port, true).await {
                Ok(imposter) => report.deleted.push(imposter.current_config()),
                // A concurrent delete got there first: already gone, which is what was asked.
                Err(ImposterError::NotFound(_)) => {
                    debug!("delete_all: imposter on port {port} was already gone");
                }
                Err(e) => report.failed.push((port, e)),
            }
        }

        if report.failed.is_empty() {
            self.emit(ImposterEvent::AllDeleted);
        } else {
            for port in report.deleted.iter().filter_map(|config| config.port) {
                self.emit(ImposterEvent::Deleted(port));
            }
        }
        report
    }

    /// Replace all imposters with a fresh set (issue #197 hot-reload). The whole set is validated
    /// — parseable already (the caller parsed it), plus protocol validity and no duplicate ports
    /// here — **before** the running imposters are torn down, so an invalid config leaves them
    /// intact rather than half-applied. Once validation passes, the old imposters are removed
    /// (releasing their ports) and the new ones created; in-flight requests against the old
    /// imposters complete naturally (each is held behind its own `Arc`). A residual bind failure
    /// after teardown (e.g. a port grabbed by an external process) is returned and may leave a
    /// partial set — the caller surfaces it as a 5xx. Reload resets all imposter state (recorded
    /// requests, scenario state, response cyclers).
    #[deprecated(
        note = "use `apply_config`, which reconciles incrementally and preserves unchanged imposters' runtime state"
    )]
    pub async fn reload(&self, mut configs: Vec<ImposterConfig>) -> Result<(), ImposterError> {
        Self::validate_config_set(&configs)?;
        self.refuse_reserved_in_set(&configs)?;
        Self::explicit_ports_first(&mut configs);

        // An imposter whose datadir file could not be removed is still serving (issue #1124); creating
        // the new set beside it would report a clean reload over a stale imposter.
        if let Some((_, e)) = self.delete_all().await.failed.into_iter().next() {
            return Err(e);
        }
        for config in configs {
            self.create_imposter(config).await?;
        }
        Ok(())
    }

    /// Full-set validation shared by `reload` and `apply_config`: protocol validity, no
    /// duplicate explicit ports (`0` is auto-assigned, not explicit — issue #1104), and no
    /// duplicate explicit stub ids within an imposter
    /// (the invariant `add_stub_unique` enforces incrementally, issue #202 — duplicate ids
    /// would silently corrupt the stub-key diff). Runs before anything mutates.
    fn validate_config_set<'a>(
        configs: impl IntoIterator<Item = &'a ImposterConfig>,
    ) -> Result<(), ImposterError> {
        let mut seen = std::collections::HashSet::new();
        for config in configs {
            match config.protocol.as_str() {
                "http" | "https" => {}
                other => return Err(ImposterError::InvalidProtocol(other.to_string())),
            }
            if let Some(port) = config.explicit_port()
                && !seen.insert(port)
            {
                return Err(ImposterError::PortInUse(port));
            }
            let mut ids = std::collections::HashSet::new();
            for stub in &config.stubs {
                if let Some(id) = stub.id.as_deref()
                    && !ids.insert(id)
                {
                    return Err(ImposterError::StubIdConflict(id.to_string()));
                }
            }
        }
        Ok(())
    }

    /// Reconcile the running imposters toward `desired` incrementally (issue #316): per-port
    /// diff, then an order-aware per-stub edit (stub identity = explicit id or content key,
    /// see [`super::stub_key`]) applied in place. Unlike [`reload`](Self::reload), untouched
    /// imposters keep all runtime state (recorded requests, scenario state, response cyclers)
    /// and their listeners are never torn down.
    ///
    /// Semantics per desired port: new port → create; missing port → delete; identical
    /// config → untouched; imposter-level field change or a degenerate stub diff (> 50 % of
    /// stubs changing) → wholesale replace (PUT semantics, state resets); otherwise the stub
    /// set is patched in place and untouched slots keep their cycling state. Exception
    /// (issue #817): a diff whose only imposter-level change is `enabled` toggles in place —
    /// pausing an imposter must never reset its runtime state — and is reported under
    /// [`ApplyReport::toggled`].
    ///
    /// The whole set is validated up front — `Err` means nothing was mutated. Per-port apply
    /// failures after that (e.g. a bind failure on a freed port) land in
    /// [`ApplyReport::failed`] while the remaining ports are still applied. Configs without
    /// an explicit port (absent or `0`) are never reconciled — each apply creates them fresh on an
    /// auto-assigned port (and reports their failures under port `0`). They are applied after every
    /// explicit-port config, so an auto-assigned port never takes a port an explicit imposter in the
    /// set is serving (issue #1112); [`ApplyReport::created`] therefore lists explicit ports first,
    /// in input order.
    pub async fn apply_config(
        &self,
        desired: Vec<ImposterConfig>,
    ) -> Result<ApplyReport, ImposterError> {
        self.apply_entries(desired.into_iter().map(|config| (config, None)).collect())
            .await
    }

    /// [`apply_config`](Self::apply_config) over a desired set whose entries say which store each
    /// imposter belongs to (issue #1122): `POST /admin/reload` with both a source set and a datadir
    /// applies the union, with the sources' imposters [`Persistence::Ephemeral`].
    ///
    /// A running imposter whose store changed is retagged in place, keeping its runtime state; one
    /// moving to [`Persistence::Datadir`] is written at once, and one moving to
    /// [`Persistence::Ephemeral`] is no longer written (its file, if any, is left alone).
    pub async fn apply_desired(
        &self,
        desired: Vec<DesiredImposter>,
    ) -> Result<ApplyReport, ImposterError> {
        self.apply_entries(
            desired
                .into_iter()
                .map(|d| (d.config, Some(d.persistence)))
                .collect(),
        )
        .await
    }

    /// The reconcile behind both apply entry points. `None` keeps a running imposter in the store it
    /// is in and creates a new one as [`Persistence::Datadir`]: `apply_config` is the admin API's
    /// and an embedder's door, and a `PUT /imposters` that repeats a config-file imposter must not
    /// move it into the datadir, where it would collide with its own source on the next reload.
    async fn apply_entries(
        &self,
        mut desired: Vec<(ImposterConfig, Option<Persistence>)>,
    ) -> Result<ApplyReport, ImposterError> {
        Self::validate_config_set(desired.iter().map(|(config, _)| config))?;
        self.refuse_reserved_in_set(desired.iter().map(|(config, _)| config))?;

        let mut report = ApplyReport::default();

        // Deletes first, so ports freed here can be re-bound by creates below.
        let desired_ports: std::collections::HashSet<u16> = desired
            .iter()
            .filter_map(|(config, _)| config.explicit_port())
            .collect();
        // `ports()` already scans ascending, so no separate sort is needed here (was needed for
        // the old HashMap's arbitrary key order).
        let removed_ports: Vec<u16> = self
            .imposters
            .ports()
            .into_iter()
            .filter(|port| !desired_ports.contains(port))
            .collect();
        for port in removed_ports {
            match self.delete_imposter_inner(port, true).await {
                Ok(_) => {
                    report.deleted.push(port);
                    self.emit(ImposterEvent::Deleted(port));
                }
                Err(e) => report.failed.push((port, e)),
            }
        }

        desired.sort_by_key(|(config, _)| config.explicit_port().is_none());
        for (config, requested) in desired {
            let Some(port) = config.explicit_port() else {
                // No explicit port (absent or `0`) → nothing to reconcile against; always an
                // auto-assigned create.
                let persistence = requested.unwrap_or(Persistence::Datadir);
                self.create_for_apply(config, 0, persistence, &mut report)
                    .await;
                continue;
            };

            self.apply_entry(port, config, requested, &mut report).await;
        }

        Ok(report)
    }

    /// Reconcile exactly one imposter toward `config`, making the decisions
    /// [`apply_config`](Self::apply_config) makes for its port (issue #1253): absent → create;
    /// imposter-level change → replace; registered without a listener → rebind; `enabled` differs →
    /// toggle in place; stubs differ → patch in place, or replace when the change is degenerate. No
    /// other imposter is read or written and nothing is deleted — remove a port with
    /// [`delete_imposter`](Self::delete_imposter).
    ///
    /// `config` must name an explicit port: a port-less or port-`0` config is refused with
    /// [`ImposterError::ExplicitPortRequired`], since there is nothing to reconcile it against (use
    /// `apply_config` for auto-assigned imposters). A running imposter stays in its persistence store
    /// and a new one is created as [`Persistence::Datadir`], as with `apply_config`.
    ///
    /// `Err` (an invalid protocol, a duplicate stub id, a missing port) means nothing was mutated;
    /// a failure while applying lands in [`ApplyReport::failed`], as with `apply_config`. Nothing
    /// serializes this against a concurrent `apply_config`, whose delete sweep may remove the port.
    pub async fn apply_one(&self, config: ImposterConfig) -> Result<ApplyReport, ImposterError> {
        self.apply_one_entry(config, None).await
    }

    /// [`apply_one`](Self::apply_one) with the store the imposter belongs to, the per-entry
    /// contract of [`apply_desired`](Self::apply_desired): a running imposter whose store changed is
    /// retagged in place.
    pub async fn apply_one_desired(
        &self,
        desired: DesiredImposter,
    ) -> Result<ApplyReport, ImposterError> {
        self.apply_one_entry(desired.config, Some(desired.persistence))
            .await
    }

    async fn apply_one_entry(
        &self,
        config: ImposterConfig,
        requested: Option<Persistence>,
    ) -> Result<ApplyReport, ImposterError> {
        let port = config
            .explicit_port()
            .ok_or(ImposterError::ExplicitPortRequired)?;
        Self::validate_config_set(std::iter::once(&config))?;
        self.refuse_reserved_in_set(std::iter::once(&config))?;
        let mut report = ApplyReport::default();
        self.apply_entry(port, config, requested, &mut report).await;
        Ok(report)
    }

    /// The per-port arm shared by [`apply_entries`](Self::apply_entries) and
    /// [`apply_one`](Self::apply_one), so the two cannot diverge. `config` is already validated and
    /// `port` is its explicit port.
    async fn apply_entry(
        &self,
        port: u16,
        mut config: ImposterConfig,
        requested: Option<Persistence>,
        report: &mut ApplyReport,
    ) {
        let Ok(existing) = self.get_imposter(port) else {
            let persistence = requested.unwrap_or(Persistence::Datadir);
            self.create_for_apply(config, port, persistence, report)
                .await;
            return;
        };

        // Retag before anything below can write the file, so every write in this iteration
        // (the enabled toggle, a stub patch, a replace's re-create) goes to the new store.
        let persistence = requested.unwrap_or_else(|| existing.persistence());
        let retagged = existing.persistence() != persistence;
        if retagged {
            existing.set_persistence(persistence);
        }

        if imposter_level_differs_ignoring_enabled(&existing, &mut config) {
            self.replace_imposter(port, config, persistence, requested, report)
                .await;
            return;
        }

        // Heal an imposter registered without a listener (issue #143). Mandatory, not an
        // optimization: before that feature a failed bind left no entry, so every apply
        // naturally retried it; with an entry present the arms below would see "exists,
        // unchanged" and the node would stay degraded forever even after the port freed up.
        // Reported like a create/failure so the embedder's degraded-state tracking — which
        // keys off exactly these fields — clears or refreshes on the same apply.
        if !existing.is_bound() {
            match self.rebind_imposter(&existing).await {
                Ok(()) => report.created.push(port),
                Err(e) => report.failed.push((port, e)),
            }
        }

        // An enabled-only imposter-level diff applies IN PLACE (issue
        // #817): pause/resume must never reset cursors, scenario state or
        // recorded requests, which a wholesale replace would. Compared
        // against the live flag, not the retained boot value.
        if existing.is_enabled() != config.enabled {
            // `toggled` means applied in place: a failed toggle is reported in `failed` alone.
            match self.set_imposter_enabled(port, config.enabled).await {
                Ok(()) => report.toggled.push(port),
                Err(e) => report.failed.push((port, e)),
            }
        }

        match existing.reconcile_stubs(&config.stubs) {
            StubReconcile::Unchanged => {
                if retagged
                    && persistence == Persistence::Datadir
                    && let Err(e) = self.persist_imposter_checked(&existing).await
                {
                    report.failed.push((port, e));
                }
            }
            StubReconcile::Patched { removed_keys } => {
                // apply_config removals are stub deletes: fire the sequencer GC hook
                // per removed stub, same as delete_stub (issue #313).
                if let Some(sequencer) = &self.sequencer {
                    for key in &removed_keys {
                        sequencer.reset_scope(port, Some(key));
                    }
                }
                report.stub_patched.push(port);
                self.emit(ImposterEvent::StubsChanged(port));
                // The in-memory patch stands either way; a datadir write failure must
                // still be observable (issue #173), not silently lost until restart.
                if let Err(e) = self.persist_imposter_checked(&existing).await {
                    report.failed.push((port, e));
                }
            }
            StubReconcile::Degenerate => {
                self.replace_imposter(port, config, persistence, requested, report)
                    .await;
            }
        }
    }

    /// Stable-sort `configs` so every explicit-port config comes before any auto-assigned one (issue
    /// #1112). The kernel picks an auto-assigned port (#1501) and can hand out one a later config in
    /// the same set names, so a port-less create that runs first could take it. With explicit ports created (or
    /// replaced, which re-binds them) first, auto-assign skips every one that is serving. An explicit
    /// create that fails leaves its port free, and that failure is reported.
    pub fn explicit_ports_first(configs: &mut [ImposterConfig]) {
        configs.sort_by_key(|config| config.explicit_port().is_none());
    }

    /// Create for `apply_config`: record the assigned port + Created event, or a failure
    /// under `fail_port` (the sentinel `0` for port-less, auto-assigned configs).
    ///
    /// Honours [`Self::with_serve_unbound`] (issue #143): when the port could not be bound the
    /// imposter is still registered and still emits `Created` — it is in the map and answers every
    /// in-process route, so suppressing the event would make the event stream disagree with what
    /// `get_imposter` returns — but the port is reported under `failed`, never `created`, so an
    /// embedder tracking degraded state off the report sees the failure rather than a clean create.
    async fn create_for_apply(
        &self,
        config: ImposterConfig,
        fail_port: u16,
        persistence: Persistence,
        report: &mut ApplyReport,
    ) {
        match self
            .create_imposter_staged(config, self.serve_unbound, persistence)
            .await
        {
            Ok((assigned, bind_failure)) => {
                match bind_failure {
                    None => report.created.push(assigned),
                    Some(e) => report.failed.push((assigned, e)),
                }
                self.emit(ImposterEvent::Created(assigned));
            }
            Err(e) => report.failed.push((fail_port, e)),
        }
    }

    /// Wholesale replace (PUT semantics): tear down, then recreate; all runtime state resets.
    /// When the recreate fails after a successful teardown, the imposter is genuinely gone —
    /// that is reported as deleted (list + event) alongside the failure, so listeners and the
    /// report never track a phantom imposter.
    ///
    /// This is an apply-path create too, so it honours [`Self::with_serve_unbound`] (issue #143).
    /// It has to: without it, editing an imposter whose port is squatted would delete the entry and
    /// the node would silently stop serving the imposter it had been serving in-process — the exact
    /// regression the flag exists to prevent, reintroduced through the replace path.
    ///
    /// `requested` is the apply entry's store: `Some` when the file was read from the datadir as
    /// this apply's input (a reload), `None` when the file is only a copy of runtime state.
    async fn replace_imposter(
        &self,
        port: u16,
        config: ImposterConfig,
        persistence: Persistence,
        requested: Option<Persistence>,
        report: &mut ApplyReport,
    ) {
        // `false`: the re-create overwrites `{port}.json`, so unlinking it first only opens a window
        // in which a failed re-create loses the file — the operator's own file, on a datadir reload.
        if let Err(e) = self.delete_imposter_inner(port, false).await {
            report.failed.push((port, e));
            return;
        }
        match self
            .create_imposter_staged(config, self.serve_unbound, persistence)
            .await
        {
            Ok((_, bind_failure)) => {
                match bind_failure {
                    None => report.replaced.push(port),
                    // Replaced *and* degraded: the new config is what serves now, but the port is
                    // still unbound, so the failure — not a clean `replaced` — is what the report
                    // carries.
                    Some(e) => report.failed.push((port, e)),
                }
                self.emit(ImposterEvent::Replaced(port));
            }
            Err(e) => {
                // Reported deleted, so a runtime copy must go too, or a restart would bring back the
                // imposter this apply replaced. A file that was the apply's input stays, and so does
                // one a concurrent create on this port has just written (`PortInUse`).
                if requested.is_none()
                    && persistence == Persistence::Datadir
                    && !matches!(e, ImposterError::PortInUse(_))
                    && let Err(cleanup) = self.remove_persisted_file_if_vacant(port).await
                {
                    report.failed.push((port, cleanup));
                }
                report.deleted.push(port);
                self.emit(ImposterEvent::Deleted(port));
                report.failed.push((port, e));
            }
        }
    }

    /// Get imposter count (for future metrics)
    pub fn count(&self) -> usize {
        self.imposters.len()
    }

    /// Add stub to an imposter
    pub async fn add_stub(
        &self,
        port: u16,
        stub: Stub,
        index: Option<usize>,
    ) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        // Reject a duplicate `id` atomically (issue #202); stubs without an id are unaffected.
        let id = stub.id.clone();
        if !imposter.add_stub_unique(stub, index) {
            return Err(ImposterError::StubIdConflict(id.unwrap_or_default()));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Replace the stub addressed by `id` in place (issue #202), preserving its position.
    pub async fn replace_stub_by_id(
        &self,
        port: u16,
        id: &str,
        stub: Stub,
    ) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        if !imposter.replace_stub_by_id(id, stub) {
            return Err(ImposterError::StubNotFound(id.to_string()));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Append `responses` to the stub addressed by `id` as a `proxyAlways` recording does (issue
    /// #1513): they join the tail of its current response cycle and its cursor is kept, where
    /// [`replace_stub_by_id`](Self::replace_stub_by_id) with the extended list would serve them in
    /// display order.
    pub async fn append_stub_responses_by_id(
        &self,
        port: u16,
        id: &str,
        responses: Vec<StubResponse>,
    ) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        if !imposter.append_stub_responses_by_id(id, responses) {
            return Err(ImposterError::StubNotFound(id.to_string()));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Delete the stub addressed by `id` (issue #202).
    pub async fn delete_stub_by_id(&self, port: u16, id: &str) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        if !imposter.delete_stub_by_id(id) {
            return Err(ImposterError::StubNotFound(id.to_string()));
        }
        if let Some(sequencer) = &self.sequencer {
            sequencer.reset_scope(port, Some(id));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Get the stub addressed by `id` (issue #202).
    pub fn get_stub_by_id(&self, port: u16, id: &str) -> Result<Stub, ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter
            .get_stub_by_id(id)
            .ok_or_else(|| ImposterError::StubNotFound(id.to_string()))
    }

    /// Tear down a correlation space (issue #223): remove its scoped stubs, recorded requests,
    /// and scenario state, then persist the updated stub set.
    pub async fn teardown_space(&self, port: u16, space: &str) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter
            .teardown_space(space)
            .map_err(ImposterError::Backend)?;
        self.persist_imposter_checked(&imposter).await
    }

    /// Replace a stub
    pub async fn replace_stub(
        &self,
        port: u16,
        index: usize,
        stub: Stub,
    ) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        // An index-addressed replace restarts the stub's cycle, as Mountebank's
        // `overwriteAtIndex` does (#1515); the old key's cursor goes with it, as on a delete.
        let replaced_key = if self.sequencer.is_some() {
            imposter
                .get_stub(index)
                .map(|stub| crate::imposter::reconcile::stub_key(&stub, 0))
        } else {
            None
        };
        imposter.replace_stub(index, stub)?;
        if let (Some(sequencer), Some(key)) = (&self.sequencer, replaced_key) {
            sequencer.reset_scope(port, Some(&key));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Delete a stub
    pub async fn delete_stub(&self, port: u16, index: usize) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        // Resolve the stub's stable key before it is gone, for the sequencer GC hook.
        let deleted_key = if self.sequencer.is_some() {
            imposter
                .get_stub(index)
                .map(|stub| crate::imposter::reconcile::stub_key(&stub, 0))
        } else {
            None
        };
        imposter.delete_stub(index)?;
        if let (Some(sequencer), Some(key)) = (&self.sequencer, deleted_key) {
            sequencer.reset_scope(port, Some(&key));
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Clear an imposter's saved proxy responses (`DELETE /imposters/{port}/savedProxyResponses`),
    /// as [`Imposter::clear_proxy_responses`] describes. Every cycle restarts, so a sequencer's
    /// cursors are reset for the whole port, as a bulk replace resets them. Only a changed stub set
    /// is announced and persisted: a cursor restart alone is not a config change.
    pub async fn clear_proxy_responses(&self, port: u16) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        let cleared = imposter.clear_proxy_responses();
        if let Some(sequencer) = &self.sequencer {
            sequencer.reset_scope(port, None);
        }
        if !cleared.changed {
            return Ok(());
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Give a running imposter the runtime state a fresh create would give it (Mountebank recreates
    /// every imposter on `PUT /imposters`, issue #1518), without touching its listener, config,
    /// persistence or `Last-Modified` stamps: response cycles and the sequencer's cursors, recorded
    /// requests and `numberOfRequests`, saved `proxyOnce` responses, injection state, and the flow
    /// and scenario state of the built-in flow store. A
    /// [`FlowStoreProvider`](crate::extensions::flow_state::FlowStoreProvider)'s store is left alone.
    ///
    /// Writes no datadir file and emits no event: the config did not change.
    pub fn restart_runtime_state(&self, port: u16) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.restart_cycles();
        if let Some(sequencer) = &self.sequencer {
            sequencer.reset_scope(port, None);
        }
        imposter
            .clear_recorded_requests()
            .map_err(ImposterError::Backend)?;
        imposter.proxy_store.clear(port);
        #[cfg(feature = "javascript")]
        crate::scripting::clear_imposter_state(port);
        imposter
            .restart_flow_state()
            .map_err(ImposterError::Backend)
    }

    /// Replace all stubs for an imposter
    pub async fn replace_stubs(&self, port: u16, stubs: Vec<Stub>) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.replace_stubs(stubs);
        if let Some(sequencer) = &self.sequencer {
            sequencer.reset_scope(port, None);
        }
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Move the stub at `from` to position `to` (issue #316), preserving the slot's
    /// cycling state. Stub order is match priority.
    pub async fn move_stub(&self, port: u16, from: usize, to: usize) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.move_stub(from, to)?;
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Enable or disable serving on `port` — a *config* write, not an
    /// ephemeral toggle (issue #817): flips the runtime flag (the data plane
    /// reacts immediately), emits [`ImposterEvent::EnabledChanged`], and
    /// persists, so a restart with `--datadir` restores the operator's last
    /// decision. Idempotent: re-asserting the current state still persists and
    /// emits (an at-least-once contract is simpler than a read-modify-check).
    pub async fn set_imposter_enabled(
        &self,
        port: u16,
        enabled: bool,
    ) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.set_enabled(enabled);
        self.emit(ImposterEvent::EnabledChanged { port, enabled });
        self.persist_imposter_checked(&imposter).await
    }

    /// Get a specific stub by index
    pub fn get_stub(&self, port: u16, index: usize) -> Result<Stub, ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter
            .get_stub(index)
            .ok_or(ImposterError::StubIndexOutOfBounds(index))
    }

    /// Shutdown all imposters (for future graceful shutdown)
    pub async fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
        let report = self.delete_all().await;
        for (port, e) in &report.failed {
            error!("shutdown: imposter on port {port} could not be deleted: {e}");
        }
    }

    /// Persist an imposter's current config to datadir (if configured).
    /// Awaits the write and returns an error if it fails, so the caller can
    /// surface a 503 to the API client instead of silently losing the change.
    async fn persist_imposter_checked(&self, imposter: &Imposter) -> Result<(), ImposterError> {
        let Some(ref datadir) = self.datadir else {
            return Ok(());
        };
        if imposter.persistence() == Persistence::Ephemeral {
            return Ok(());
        }
        let Some(port) = imposter.config.port else {
            return Ok(());
        };
        let persist = Arc::clone(&self.persist_lock).lock_owned().await;
        // A delete that ran first has removed both the file and the imposter; writing now would bring
        // back, on the next restart, an imposter that no longer exists (issue #1124).
        if !self
            .imposters
            .get(port)
            .is_some_and(|current| std::ptr::eq(Arc::as_ptr(&current), imposter))
        {
            return Ok(());
        }
        let mut snapshot = imposter.config.clone();
        snapshot.stubs = imposter.get_stubs();
        // The atomic is the runtime truth; the retained config only holds the
        // boot value. Snapshot the flag so every persist path (stub CRUD
        // included) writes the operator's current decision.
        snapshot.enabled = imposter.is_enabled();
        let json = serde_json::to_string_pretty(&snapshot).map_err(|e| {
            ImposterError::PersistError(
                anyhow::Error::new(e).context(format!("Failed to serialize imposter {port}")),
            )
        })?;
        // Replaced, never rewritten in place: a failure leaves the previous file whole (#1158).
        datadir_file::write_replacing(datadir, port, json.into_bytes(), persist)
            .await
            .map_err(|e| {
                let path = datadir_file::data_path(datadir, port);
                ImposterError::PersistError(
                    anyhow::Error::new(e)
                        .context(format!("Failed to write imposter {port} to {path:?}")),
                )
            })
    }

    /// Remove an imposter's file from datadir (if configured and the imposter belongs there).
    ///
    /// Awaited, not spawned: a delete that returned before its unlink ran let a following create on
    /// the same port write `{port}.json` first, and the late unlink then deleted the new file.
    async fn remove_persisted_imposter(&self, imposter: &Imposter) -> Result<(), ImposterError> {
        if imposter.persistence() == Persistence::Ephemeral {
            return Ok(());
        }
        match imposter.config.port {
            Some(port) => self.remove_persisted_file(port).await,
            None => Ok(()),
        }
    }

    /// Remove `{port}.json` from the datadir (if configured), whatever store the imposter was in,
    /// and any `{port}.json.tmp` a crash left beside it, so a deleted imposter leaves nothing behind.
    ///
    /// Callers must hold `persist_lock`: unlocked, the temp removal could land inside a concurrent
    /// write and fail its rename.
    async fn remove_persisted_file(&self, port: u16) -> Result<(), ImposterError> {
        let Some(ref datadir) = self.datadir else {
            return Ok(());
        };
        // The leftover first: if it cannot be removed, the delete fails with `{port}.json` still in
        // place, so `Err` keeps meaning "not deleted" (issue #1124).
        for path in [
            datadir_file::temp_path(datadir, port),
            datadir_file::data_path(datadir, port),
        ] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                // An absent file is the desired end state, not a failure: the imposter may never
                // have been persisted, or the file was already removed. Handling NotFound here
                // (rather than pre-checking `exists()`) also closes the TOCTOU window between
                // check and unlink.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(ImposterError::PersistError(anyhow::Error::new(e).context(
                        format!("Failed to remove imposter {port} file {path:?}"),
                    )));
                }
            }
        }
        Ok(())
    }

    /// `remove_persisted_file`, unless the port has been taken again by the time the lock is held.
    ///
    /// For a rollback after a failed create: the failed imposter is already out of the map, so a
    /// concurrent create on the same port can have persisted its own `{port}.json` since — and an
    /// unlocked, unconditional unlink would then delete it, silently.
    async fn remove_persisted_file_if_vacant(&self, port: u16) -> Result<(), ImposterError> {
        let _persist = self.persist_lock.lock().await;
        if self.imposters.get(port).is_some() {
            return Ok(());
        }
        self.remove_persisted_file(port).await
    }
}

impl Default for ImposterManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Imposter-level diff for `apply_config`, comparing the configs with stubs stripped and the
/// `enabled` flag normalized away: any *other* difference (protocol, TLS, recordRequests,
/// name, …) replaces wholesale, while an enabled-only diff is applied in place by the caller
/// (issue #817 — pause/resume must not reset runtime state). A serialization failure on
/// either side counts as "differs" — the conservative direction (worst case an unnecessary
/// replace, never a silently skipped change).
fn imposter_level_differs_ignoring_enabled(
    existing: &Imposter,
    desired: &mut ImposterConfig,
) -> bool {
    match (
        existing.imposter_level_view(),
        super::core::imposter_level_view(desired),
    ) {
        (Ok(running), Ok(wanted)) => *running != wanted,
        (running, wanted) => {
            error!(
                "imposter config serialization failed during reconcile; treating as changed: {:?} {:?}",
                running.as_ref().err(),
                wanted.err()
            );
            true
        }
    }
}

#[cfg(test)]
mod port_table_tests {
    use super::*;
    use serde_json::json;

    fn imposter(port: u16) -> Arc<Imposter> {
        let cfg = serde_json::from_value(json!({ "port": port, "protocol": "http", "stubs": [] }))
            .expect("valid imposter config");
        Arc::new(Imposter::new(cfg).expect("test imposter"))
    }

    // A lookup returns exactly what was claimed, and a missing port is None — the wait-free read
    // path (issue #713), replacing the RwLock<HashMap> read.
    #[test]
    fn claim_then_get_and_miss() {
        let table = PortTable::new();
        let imp = imposter(8080);
        assert!(table.try_claim(8080, Arc::clone(&imp)).is_ok());
        assert!(Arc::ptr_eq(&table.get(8080).expect("present"), &imp));
        assert!(table.get(9090).is_none(), "an unclaimed port is a miss");
        assert!(table.contains(8080));
        assert!(!table.contains(9090));
    }

    // The duplicate-port claim is rejected — the semantics the lock-serialized check-then-insert
    // guaranteed (manager create returns PortInUse). The first claim wins; the second is refused
    // and does NOT overwrite it.
    #[test]
    fn duplicate_claim_is_rejected() {
        let table = PortTable::new();
        let first = imposter(3000);
        let second = imposter(3000);
        assert!(table.try_claim(3000, Arc::clone(&first)).is_ok());
        assert!(
            table.try_claim(3000, Arc::clone(&second)).is_err(),
            "a second claim on an occupied port must be refused"
        );
        assert!(
            Arc::ptr_eq(&table.get(3000).expect("present"), &first),
            "the first claim must be preserved, not overwritten by the rejected second"
        );
    }

    // Remove frees the slot (a re-claim then succeeds) and returns the removed imposter; removing
    // an empty slot is None.
    #[test]
    fn remove_frees_the_slot() {
        let table = PortTable::new();
        let imp = imposter(4000);
        table.try_claim(4000, Arc::clone(&imp)).expect("claim");
        let removed = table.remove(4000).expect("removed");
        assert!(Arc::ptr_eq(&removed, &imp));
        assert!(table.get(4000).is_none(), "slot is empty after remove");
        assert!(
            table.remove(4000).is_none(),
            "removing an empty slot is None"
        );
        // The slot is reusable.
        assert!(table.try_claim(4000, imposter(4000)).is_ok());
    }

    // `count` tracks live entries across claims and removes (backs the admin `len`).
    #[test]
    fn count_tracks_live_entries() {
        let table = PortTable::new();
        assert_eq!(table.len(), 0);
        table.try_claim(1, imposter(1)).expect("claim");
        table.try_claim(2, imposter(2)).expect("claim");
        assert_eq!(table.len(), 2);
        // A rejected duplicate must not inflate the count.
        assert!(table.try_claim(1, imposter(1)).is_err());
        assert_eq!(table.len(), 2, "a rejected claim does not change the count");
        table.remove(1).expect("removed");
        assert_eq!(table.len(), 1);
    }

    // Iteration yields ports (and imposters) in ASCENDING port order — deterministic, unlike the
    // old HashMap's arbitrary order. Claims are made out of order to prove the ordering is the
    // scan's, not insertion's.
    #[test]
    fn iteration_is_ascending_by_port() {
        let table = PortTable::new();
        for p in [9000u16, 80, 65535, 443, 1] {
            table.try_claim(p, imposter(p)).expect("claim");
        }
        assert_eq!(table.ports(), vec![1, 80, 443, 9000, 65535]);
        let ports_from_imposters: Vec<u16> = table
            .imposters()
            .iter()
            .filter_map(|i| i.config.port)
            .collect();
        assert_eq!(ports_from_imposters, vec![1, 80, 443, 9000, 65535]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposter::ResponseMode;
    use parking_lot::Mutex;

    /// Raise this process's soft file-descriptor limit for the fd-heavy concurrent tests (#814).
    ///
    /// Several tests in this module open many sockets/runtimes at once; at `--test-threads=16` a
    /// small launcher default (macOS launchd defaults `RLIMIT_NOFILE` soft to 256) is exhausted
    /// (`EMFILE`) and they fail spuriously — the failure is resource contention, not a real bug.
    /// Raise the soft limit toward the hard cap (bounded at 4096), once per process. Best-effort:
    /// if the raise is refused the test still runs and surfaces `EMFILE` loudly on its own.
    #[cfg(unix)]
    fn raise_fd_limit() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // SAFETY: get/setrlimit with a valid resource id and a stack-allocated rlimit; the
            // calls read/write only that struct and report failure via their return value.
            unsafe {
                let mut lim = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
                    let target = lim.rlim_max.min(4096);
                    if lim.rlim_cur < target {
                        lim.rlim_cur = target;
                        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                            eprintln!(
                                "raise_fd_limit: setrlimit to {target} refused: {}",
                                std::io::Error::last_os_error()
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "raise_fd_limit: getrlimit failed: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        });
    }

    #[cfg(not(unix))]
    fn raise_fd_limit() {}

    /// Fixed port, only for a test that must name a port before the manager binds it (#1485).
    /// Probes the wildcard a host-less imposter binds (dual-stack `[::]`, #1514), so a
    /// concurrent socket on the same number cannot take this test's connections.
    fn free_port() -> u16 {
        dual_stack_holder().local_addr().expect("local addr").port()
    }

    /// A plain listener on a fresh port of the wildcard a host-less imposter binds: `[::]`, which
    /// is dual-stack by default on macOS and Linux (#1514), else `0.0.0.0` on a host without IPv6.
    /// An IPv4-only holder no longer conflicts on macOS, where a dual-stack reusable bind beside it
    /// succeeds.
    fn dual_stack_holder() -> std::net::TcpListener {
        std::net::TcpListener::bind("[::]:0")
            .or_else(|_| std::net::TcpListener::bind("0.0.0.0:0"))
            .expect("bind an ephemeral port")
    }

    // ── Accept-error handling (issue #750) ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_create_imposter_writes_to_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let file = dir.path().join(format!("{port_0}.json"));
        assert!(file.exists(), "imposter file should be written to datadir");

        let content = std::fs::read_to_string(&file).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["port"], port_0);
        assert_eq!(json["protocol"], "http");

        manager.delete_imposter(port_0).await.unwrap();
    }

    // Issue #1519: a clear that changes no stub only restarts cursors, which is not a config change.
    #[tokio::test]
    async fn a_clear_that_changes_no_stub_writes_nothing_to_the_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"body": "a"}}, {"is": {"body": "b"}}]}]
        }))
        .expect("config");
        let port_0 = manager.create_imposter(config).await.expect("create");
        let file = dir.path().join(format!("{port_0}.json"));
        std::fs::remove_file(&file).expect("the create wrote the file");

        manager.clear_proxy_responses(port_0).await.expect("clear");
        assert!(!file.exists(), "nothing changed, so nothing is persisted");

        manager.delete_imposter(port_0).await.expect("delete");
    }

    // Issue #1181: a behaviors block on a proxy response used to be erased from the persisted file,
    // so a restart silently lost config the author wrote.
    #[tokio::test]
    async fn a_behaviors_block_on_a_proxy_response_survives_the_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{
                "proxy": {"to": "http://127.0.0.1:1"},
                "_behaviors": {"wait": 500}
            }]}]
        }))
        .expect("config");

        let port_0 = manager.create_imposter(config).await.expect("create");
        let content =
            std::fs::read_to_string(dir.path().join(format!("{port_0}.json"))).expect("file");
        let json: serde_json::Value = serde_json::from_str(&content).expect("json");
        assert_eq!(
            json["stubs"][0]["responses"][0]["behaviors"],
            serde_json::json!([{"wait": 500}]),
            "{json}"
        );

        manager.delete_imposter(port_0).await.expect("delete");
    }

    #[tokio::test]
    async fn test_delete_imposter_removes_from_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let file = dir.path().join(format!("{port_0}.json"));
        assert!(file.exists(), "file should exist after create");

        manager.delete_imposter(port_0).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!file.exists(), "file should be removed after delete");
    }

    /// Deleting an imposter whose datadir file is already gone must still succeed: an absent file
    /// is the desired end state, not a failure (issue #564). Pins that dropping the `exists()`
    /// pre-check did not turn a missing file into a surfaced error.
    #[tokio::test]
    async fn test_delete_imposter_succeeds_when_datadir_file_already_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        // The port is OS-assigned and unique per run: create_imposter binds with SO_REUSEPORT, so
        // a port shared with another test or process load-balances between imposters (#814).
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Remove the file out from under the manager, so the unlink races with an absent file.
        let file = dir.path().join(format!("{port_0}.json"));
        std::fs::remove_file(&file).expect("pre-remove the persisted file");
        assert!(!file.exists(), "precondition: file is already gone");

        manager
            .delete_imposter(port_0)
            .await
            .expect("delete must succeed when the datadir file is already gone");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!file.exists(), "file must still be absent after delete");
    }

    #[tokio::test]
    async fn test_add_stub_updates_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        // The port is OS-assigned (see the sibling datadir test): a shared SO_REUSEPORT port
        // would load-balance between two imposters and flake (#814).
        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager.create_imposter(config).await.expect("create");
        // Wait for create_imposter's fire-and-forget persistence to land.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stub: Stub = serde_json::from_value(serde_json::json!({
            "predicates": [],
            "responses": [{"is": {"statusCode": 200, "body": "hello"}}]
        }))
        .unwrap();

        manager.add_stub(port_0, stub, None).await.unwrap();

        let file = dir.path().join(format!("{port_0}.json"));
        let content = std::fs::read_to_string(&file).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["stubs"].as_array().unwrap().len(), 1);

        manager.delete_imposter(port_0).await.unwrap();
    }

    fn datadir_config() -> ImposterConfig {
        serde_json::from_value(serde_json::json!({"protocol": "http", "stubs": []}))
            .expect("config")
    }

    /// Issue #1158: `{port}.json` was rewritten in place, so a reader — a `POST /admin/reload`, or
    /// the next start after a kill — could see it empty or half-written. Every read must parse.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_concurrent_reader_never_sees_a_partial_datadir_file() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");

        let file = dir.path().join(format!("{port_0}.json"));
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let stop = Arc::clone(&stop);
            tokio::task::spawn_blocking(move || {
                let (mut reads, mut partial) = (0u32, Vec::new());
                while !stop.load(Ordering::Relaxed) {
                    let bytes = std::fs::read(&file).expect("the file is always there");
                    reads += 1;
                    if serde_json::from_slice::<ImposterConfig>(&bytes).is_err() {
                        partial.push(bytes.len());
                    }
                }
                (reads, partial)
            })
        };
        // Large enough that each write takes long enough to be caught mid-way.
        let body = "x".repeat(32 * 1024);
        for _ in 0..100 {
            let stub: Stub = serde_json::from_value(serde_json::json!({
                "responses": [{"is": {"statusCode": 200, "body": body}}]
            }))
            .expect("stub");
            manager
                .add_stub(port_0, stub, None)
                .await
                .expect("add_stub");
        }
        stop.store(true, Ordering::Relaxed);
        let (reads, partial) = reader.await.expect("reader");
        manager.delete_imposter(port_0).await.expect("delete");

        assert!(reads > 0, "the reader must have read");
        assert!(
            partial.is_empty(),
            "{} of {reads} reads saw a partial document (sizes {:?})",
            partial.len(),
            &partial[..partial.len().min(5)]
        );
    }

    /// A failed persist answers the client with an error — and that must mean the previous state
    /// survived, not that the file was truncated on the way to failing (issue #1158).
    #[tokio::test]
    async fn a_failed_persist_leaves_the_previous_file_intact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        let file = dir.path().join(format!("{port_0}.json"));
        let before = std::fs::read(&file).expect("persisted");

        std::fs::create_dir(dir.path().join(format!("{port_0}.json.tmp")))
            .expect("block the temp path");
        let stub: Stub = serde_json::from_value(serde_json::json!({
            "responses": [{"is": {"statusCode": 200}}]
        }))
        .expect("stub");
        let err = manager
            .add_stub(port_0, stub, None)
            .await
            .expect_err("the write cannot happen");
        assert!(matches!(err, ImposterError::PersistError(_)), "{err:?}");
        assert_eq!(std::fs::read(&file).expect("still there"), before);

        std::fs::remove_dir(dir.path().join(format!("{port_0}.json.tmp"))).expect("unblock");
        manager.delete_imposter(port_0).await.expect("delete");
    }

    /// A create whose persist fails used to unlink `{port}.json` to clear a truncated write. With
    /// nothing truncated there is nothing to clear, and the file already there — a reload's own
    /// input, in the case that mattered — must survive the failed create.
    #[tokio::test]
    async fn a_create_that_cannot_persist_leaves_an_existing_file_alone() {
        // Fixed port: the operator's `{port}.json` must exist before the create.
        let port_0 = free_port();
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join(format!("{port_0}.json"));
        std::fs::write(&file, "operator's file").expect("seed");
        std::fs::create_dir(dir.path().join(format!("{port_0}.json.tmp")))
            .expect("block the temp path");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let err = manager
            .create_imposter(datadir_cfg(
                serde_json::json!({"port": port_0, "protocol": "http", "stubs": []}),
            ))
            .await
            .expect_err("the create cannot persist");
        assert!(matches!(err, ImposterError::PersistError(_)), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("kept"),
            "operator's file"
        );
        assert!(
            manager.get_imposter(port_0).is_err(),
            "and nothing is serving"
        );
    }

    #[tokio::test]
    async fn a_persist_leaves_no_temp_file_and_a_delete_removes_a_leftover() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        assert_eq!(
            datadir_files(dir.path()),
            vec![format!("{port_0}.json").to_string()]
        );

        // What a crash between the temp write and the rename leaves behind.
        std::fs::write(dir.path().join(format!("{port_0}.json.tmp")), "{ trunc").expect("leftover");
        manager.delete_imposter(port_0).await.expect("delete");
        assert!(
            datadir_files(dir.path()).is_empty(),
            "{:?}",
            datadir_files(dir.path())
        );
    }

    /// A delete whose leftover temp file cannot be removed must fail before touching `{port}.json`:
    /// `Err` means "not deleted" (issue #1124), and a restart must still find the imposter.
    #[tokio::test]
    async fn a_delete_that_cannot_clear_the_temp_file_keeps_the_imposter_and_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        // A non-empty directory at the temp path cannot be removed as a file.
        let blocker = dir.path().join(format!("{port_0}.json.tmp"));
        std::fs::create_dir(&blocker).expect("block");
        std::fs::write(blocker.join("x"), "").expect("fill");

        manager
            .delete_imposter(port_0)
            .await
            .expect_err("the leftover cannot be removed");
        assert!(
            dir.path().join(format!("{port_0}.json")).exists(),
            "the file must survive"
        );
        assert!(manager.get_imposter(port_0).is_ok(), "and the imposter too");

        std::fs::remove_dir_all(&blocker).expect("unblock");
        manager.delete_imposter(port_0).await.expect("delete");
    }

    fn datadir_files(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read datadir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn datadir_cfg(v: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(v).expect("test imposter config")
    }

    fn one_stub() -> Stub {
        serde_json::from_value(serde_json::json!({
            "responses": [{"is": {"statusCode": 200, "body": "hello"}}]
        }))
        .expect("stub")
    }

    // Issue #1122: an ephemeral imposter (loaded from `--configfile`/`--imposters`) is re-read from
    // its source, so neither its create nor a later stub edit may write it to the datadir.
    #[tokio::test]
    async fn ephemeral_imposter_never_touches_the_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let port_0 = manager
            .create_imposter_as(datadir_config(), Persistence::Ephemeral)
            .await
            .expect("create first");
        manager
            .create_imposter_as(datadir_config(), Persistence::Ephemeral)
            .await
            .expect("create second");
        manager
            .add_stub(port_0, one_stub(), None)
            .await
            .expect("add_stub");

        assert_eq!(manager.count(), 2);
        assert_eq!(datadir_files(dir.path()), Vec::<String>::new());

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn apply_desired_persists_only_datadir_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let entry = |persistence| DesiredImposter {
            config: datadir_config(),
            persistence,
        };
        let report = manager
            .apply_desired(vec![
                entry(Persistence::Ephemeral),
                entry(Persistence::Datadir),
                entry(Persistence::Ephemeral),
            ])
            .await
            .expect("apply");

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.created.len(), 3);
        let persisted: Vec<u16> = report
            .created
            .iter()
            .copied()
            .filter(|&p| {
                manager.get_imposter(p).expect("running").persistence() == Persistence::Datadir
            })
            .collect();
        assert_eq!(persisted.len(), 1, "{report:?}");
        assert_eq!(
            datadir_files(dir.path()),
            vec![format!("{}.json", persisted[0])]
        );

        manager.delete_all().await;
    }

    // A running imposter can change store between two applies (its source dropped it and the
    // operator put the same imposter in the datadir). That is retagged in place, never replaced,
    // and from then on it persists exactly as its new store requires.
    #[tokio::test]
    async fn apply_desired_retags_a_running_imposter_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter_as(datadir_config(), Persistence::Ephemeral)
            .await
            .expect("create");
        let config =
            || datadir_cfg(serde_json::json!({"port": port_0, "protocol": "http", "stubs": []}));
        let file = dir.path().join(format!("{port_0}.json"));
        assert!(!file.exists());

        let report = manager
            .apply_desired(vec![DesiredImposter {
                config: config(),
                persistence: Persistence::Datadir,
            }])
            .await
            .expect("retag to datadir");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(
            report.created.is_empty() && report.replaced.is_empty() && report.deleted.is_empty(),
            "a retag is not a create, replace or delete: {report:?}"
        );
        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("retag writes the file"))
                .expect("json");
        assert_eq!(persisted["port"], port_0);
        assert_eq!(persisted["stubs"], serde_json::json!([]));

        manager
            .apply_desired(vec![DesiredImposter {
                config: config(),
                persistence: Persistence::Ephemeral,
            }])
            .await
            .expect("retag to ephemeral");
        manager
            .add_stub(port_0, one_stub(), None)
            .await
            .expect("add_stub");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("file")).expect("json");
        assert_eq!(
            after["stubs"],
            serde_json::json!([]),
            "an ephemeral imposter's stub edit must not be written"
        );

        manager.delete_all().await;
    }

    // Review of #1122: the enabled toggle persists, so it must see the new tag. Before the fix it ran
    // before the retag and wrote back the file the operator had moved into the config file.
    #[tokio::test]
    async fn apply_desired_retag_with_an_enabled_change_writes_no_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        let file = dir.path().join(format!("{port_0}.json"));
        std::fs::remove_file(&file).expect("operator removes the datadir file");

        let report = manager
            .apply_desired(vec![DesiredImposter {
                config: datadir_cfg(serde_json::json!(
                    {"port": port_0, "protocol": "http", "enabled": false, "stubs": []}
                )),
                persistence: Persistence::Ephemeral,
            }])
            .await
            .expect("apply");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.toggled, vec![port_0]);
        assert!(
            !file.exists(),
            "a pause applied as ephemeral must not write the file back"
        );

        manager.delete_all().await;
    }

    // `apply_config` is the admin API's door (`PUT /imposters`). Repeating a config-file imposter
    // through it, unchanged or edited, must not move that imposter into the datadir.
    #[tokio::test]
    async fn apply_config_keeps_a_running_imposter_in_its_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter_as(datadir_config(), Persistence::Ephemeral)
            .await
            .expect("create");
        let seeded = serde_json::json!({"port": port_0, "protocol": "http", "stubs": []});

        let unchanged = manager
            .apply_config(vec![datadir_cfg(seeded)])
            .await
            .expect("apply unchanged");
        assert!(unchanged.failed.is_empty(), "{:?}", unchanged.failed);

        let renamed = manager
            .apply_config(vec![datadir_cfg(serde_json::json!(
                {"port": port_0, "protocol": "http", "name": "renamed", "stubs": []}
            ))])
            .await
            .expect("apply renamed");
        assert_eq!(renamed.replaced, vec![port_0]);

        let created = manager
            .apply_config(vec![
                datadir_cfg(serde_json::json!(
                    {"port": port_0, "protocol": "http", "name": "renamed", "stubs": []}
                )),
                datadir_config(),
            ])
            .await
            .expect("apply with a new imposter");
        assert_eq!(created.created.len(), 1, "{created:?}");
        let port_1 = created.created[0];

        assert_eq!(
            datadir_files(dir.path()),
            vec![format!("{port_1}.json").to_string()],
            "only the imposter the admin API created is persisted"
        );
        assert_eq!(
            manager.get_imposter(port_0).expect("running").persistence(),
            Persistence::Ephemeral
        );

        manager.delete_all().await;
    }

    // A replace used to unlink `{port}.json` before re-creating. When the re-create failed on a
    // datadir reload, the operator's own file was lost along with the imposter.
    #[tokio::test]
    async fn a_failed_replace_from_the_datadir_keeps_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        let file = dir.path().join(format!("{port_0}.json"));
        assert!(file.exists());

        let report = manager
            .apply_desired(vec![DesiredImposter {
                config: datadir_cfg(serde_json::json!({
                    "port": port_0, "protocol": "https",
                    "cert": "not a pem", "key": "not a pem", "stubs": []
                })),
                persistence: Persistence::Datadir,
            }])
            .await
            .expect("validation passes; the re-create fails");
        assert_eq!(report.deleted, vec![port_0], "{report:?}");
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(
            file.exists(),
            "a failed replace must not lose the datadir's file"
        );

        manager.delete_all().await;
    }

    // Through `apply_config` (`PUT /imposters`) the file is a copy of the imposter being replaced. The
    // report says it was deleted, so a restart must not bring it back.
    #[tokio::test]
    async fn a_failed_replace_through_apply_config_removes_the_stale_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(datadir_config())
            .await
            .expect("create");
        let file = dir.path().join(format!("{port_0}.json"));
        assert!(file.exists());

        let report = manager
            .apply_config(vec![datadir_cfg(serde_json::json!({
                "port": port_0, "protocol": "https",
                "cert": "not a pem", "key": "not a pem", "stubs": []
            }))])
            .await
            .expect("validation passes; the re-create fails");
        assert_eq!(report.deleted, vec![port_0], "{report:?}");
        assert!(
            !file.exists(),
            "the replaced imposter's copy must not survive a reported delete"
        );

        manager.delete_all().await;
    }

    // A stub patch and a move out of the datadir in one apply: the patch must not be written.
    #[tokio::test]
    async fn apply_desired_patch_and_retag_to_ephemeral_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let stub = |body: &str| serde_json::json!({"responses": [{"is": {"body": body}}]});

        let port_0 = manager
            .create_imposter(datadir_cfg(serde_json::json!({
                "protocol": "http",
                "stubs": [stub("a"), stub("b"), stub("c")]
            })))
            .await
            .expect("create");
        let file = dir.path().join(format!("{port_0}.json"));

        let report = manager
            .apply_desired(vec![DesiredImposter {
                config: datadir_cfg(serde_json::json!({
                    "port": port_0, "protocol": "http",
                    "stubs": [stub("a"), stub("b"), stub("c"), stub("d")]
                })),
                persistence: Persistence::Ephemeral,
            }])
            .await
            .expect("apply");
        assert_eq!(report.stub_patched, vec![port_0], "{report:?}");

        let persisted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).expect("file")).expect("json");
        assert_eq!(
            persisted["stubs"].as_array().map(Vec::len),
            Some(3),
            "the patch was applied as ephemeral and must not reach the file"
        );

        manager.delete_all().await;
    }

    #[test]
    fn test_new_has_no_datadir() {
        let manager = ImposterManager::new();
        assert!(manager.datadir.is_none());
    }

    #[test]
    fn test_with_datadir_sets_datadir() {
        let manager = ImposterManager::with_datadir(Some("/tmp/test".into()));
        assert!(manager.datadir.is_some());
    }

    // =========================================================================
    // Issue #173: persistence failures must surface as errors, not silently drop
    // =========================================================================

    #[tokio::test]
    async fn test_add_stub_returns_persist_error_on_write_failure() {
        // Create must succeed first (issue #563 made create persist synchronously), so use a
        // writable datadir, then remove it so the *subsequent* add_stub write fails and must
        // propagate ImposterError::PersistError.
        let fake_dir = tempfile::tempdir().expect("tempdir");
        let datadir = fake_dir.path().join("datadir");
        std::fs::create_dir(&datadir).expect("mk datadir");

        let manager = ImposterManager::with_datadir(Some(datadir.clone()));
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager
            .create_imposter(config)
            .await
            .expect("create should succeed with a writable datadir");

        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let stub: Stub = serde_json::from_value(serde_json::json!({
            "predicates": [],
            "responses": [{"is": {"statusCode": 200}}]
        }))
        .unwrap();

        let result = manager.add_stub(port_0, stub, None).await;
        assert!(
            matches!(result, Err(ImposterError::PersistError(_))),
            "add_stub should return PersistError when datadir is not writable, got: {result:?}"
        );

        manager.delete_imposter(port_0).await.unwrap();
    }

    // Issue #563: create-time persistence must surface as an error and roll back the imposter,
    // not silently succeed and vanish on restart (the fire-and-forget bug).
    #[tokio::test]
    async fn create_returns_persist_error_on_unwritable_datadir() {
        let fake_dir = tempfile::tempdir().expect("tempdir");
        let nonexistent_datadir = fake_dir.path().join("does_not_exist_subdir");
        let manager = ImposterManager::with_datadir(Some(nonexistent_datadir));

        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let result = manager.create_imposter(config).await;
        assert!(
            matches!(result, Err(ImposterError::PersistError(_))),
            "create must surface PersistError when the datadir is not writable, got: {result:?}"
        );
        // Rolled back: the imposter must not linger in the map.
        assert_eq!(
            manager.count(),
            0,
            "a create that failed to persist must not leave a running imposter"
        );
    }

    #[tokio::test]
    async fn create_persists_file_synchronously() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": []
        }))
        .unwrap();

        let port_0 = manager.create_imposter(config).await.expect("create");
        // No sleep: create now awaits the persist, so the file exists the moment create returns.
        assert!(
            dir.path().join(format!("{port_0}.json")).exists(),
            "create must have written the imposter file before returning"
        );

        manager.delete_imposter(port_0).await.unwrap();
    }

    // =========================================================================
    // Issue #207: DELETE must close existing keep-alive connections so a deleted
    // imposter serves no further requests on a pooled/keep-alive connection.
    // =========================================================================

    /// Read from the stream until `needle` appears or a short timeout elapses.
    async fn read_until(stream: &mut tokio::net::TcpStream, needle: &str) -> String {
        use tokio::io::AsyncReadExt;
        let mut acc = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
                .await
            {
                Ok(Ok(n)) if n > 0 => {
                    acc.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&acc).contains(needle) {
                        break;
                    }
                }
                _ => break, // timeout, read error, or EOF (0 bytes)
            }
        }
        String::from_utf8_lossy(&acc).into_owned()
    }

    #[tokio::test]
    async fn test_delete_closes_keepalive_connections() {
        // Fixed port: probed again after the delete (see the #809 note below).
        let port_0 = free_port();
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        // Issue #809: this test probes the port *after* deleting the imposter, so it takes a port
        // nothing auto-assigns from this test's manager. See `delete_blocks_until_inflight_response_drains`
        // for why the two halves of this file differ.
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": port_0,
            "stubs": [{
                "predicates": [{"equals": {"path": "/ping"}}],
                "responses": [{"is": {"statusCode": 200, "body": "pong"}}]
            }]
        }))
        .unwrap();

        let port = manager.create_imposter(config).await.expect("create");

        // Open a keep-alive connection and confirm it is served.
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();

        let first = read_until(&mut stream, "pong").await;
        assert!(
            first.contains("200") && first.contains("pong"),
            "first keep-alive request should be served, got: {first}"
        );

        // Delete the imposter. No settle: `delete` now awaits full teardown (issue #596) — the
        // accept loop has ended and established connections are drained by the time it returns — so
        // the assertions below must hold *immediately*, with no heuristic sleep.
        manager.delete_imposter(port).await.expect("delete");

        // Criterion 1: reuse the SAME connection. The deleted imposter must serve
        // nothing AND the socket must be actively closed — an empty read proves
        // EOF/close, distinguishing a real teardown from a hung connection.
        let _ = stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await;
        let after = read_until(&mut stream, "pong").await;
        assert!(
            after.is_empty(),
            "deleted imposter must close the keep-alive connection and serve nothing, got: {after:?}"
        );

        // Criterion 2: a fresh connection must not be served either — the listener
        // is gone, so connect is refused or the socket yields EOF with no body.
        match TcpStream::connect(("127.0.0.1", port)).await {
            Err(_) => {} // connection refused — listener torn down, as expected
            Ok(mut fresh) => {
                let _ = fresh
                    .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await;
                let fresh_resp = read_until(&mut fresh, "pong").await;
                assert!(
                    !fresh_resp.contains("pong"),
                    "deleted imposter must not serve a fresh connection, got: {fresh_resp:?}"
                );
            }
        }
    }

    /// A plain HTTP/1 upstream on an ephemeral port that reads each request head and answers
    /// `200 done` after `delay`, or never answers when `delay` is `None`: an in-flight response
    /// that is slow for a reason other than a `wait` behavior, which a delete aborts (issue #1616).
    async fn slow_upstream(delay: Option<std::time::Duration>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream");
        let port = listener.local_addr().expect("upstream addr").port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                            _ => return,
                        }
                    }
                    match delay {
                        Some(delay) => {
                            tokio::time::sleep(delay).await;
                            let _ = socket
                                .write_all(
                                    b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\ndone",
                                )
                                .await;
                        }
                        // Hold the socket open without answering until the test ends.
                        None => std::future::pending::<()>().await,
                    }
                });
            }
        });
        port
    }

    // Issue #596: `delete` must await the in-flight generation's quiesce, not fire-and-forget. With
    // a request in flight on a slow upstream, delete cannot return until that response has been
    // served and the connection drained — so a returning delete is proof the old generation is
    // gone. Deterministic: pre-fix `delete` returns in ~0ms; post-fix it blocks for the in-flight
    // response. Issue #1616: the slow response is a proxy, not a `wait`, which a delete now cuts
    // off; every other in-flight response still finishes (#207).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_blocks_until_inflight_response_drains() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        let upstream = slow_upstream(Some(std::time::Duration::from_millis(1500))).await;
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [{"equals": {"path": "/slow"}}],
                "responses": [{"proxy": {"to": format!("http://127.0.0.1:{upstream}")}}]
            }]
        }))
        .unwrap();
        // Issue #809: OS-assigned. A hardcoded port collides with an unrelated listener during a
        // full-workspace run (many test binaries at once) and fails the *create* — which is how
        // this test actually flaked: it panicked at `create`, not at the drain assertion below.
        //
        // Auto-assign is only safe for a test that never touches the port again after deleting it.
        // A test that re-binds or re-probes the port must keep a fixed one below the ephemeral
        // range, because `delete` returns the port to the kernel, which can hand it to a
        // concurrent test's auto-assign — see `delete_then_recreate_same_port_serves_new_generation`.
        let port = manager.create_imposter(config).await.expect("create");

        // Send a request that will spend ~1.5s on the upstream before its response is written;
        // don't read yet — the response is still in flight on this connection.
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        // Let the request reach the server and enter the wait. Issue #809: the margins here are
        // deliberately wide (1500ms wait / 400ms entry / 700ms floor rather than 400/80/250). The
        // assertion below only means anything if the request is *already waiting* when delete runs,
        // and under a saturated `cargo test --workspace` this task can be descheduled for far
        // longer than a tight margin allows — which would fail the drain assertion for a scheduling
        // reason rather than a real regression.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(port).await.expect("delete");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(700),
            "delete must block until the in-flight response drains (issue #596); returned in {elapsed:?}"
        );
        // The in-flight response was served gracefully (finished, not aborted mid-write).
        let served = read_until(&mut stream, "done").await;
        assert!(
            served.contains("200") && served.contains("done"),
            "in-flight request served gracefully before close, got: {served:?}"
        );
    }

    // Issue #596: the drain is *bounded*. A stalled in-flight request must not hang `delete` forever
    // — it returns within the drain budget (with a warning) rather than blocking on the full wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_with_stalled_request_is_bounded_by_drain() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new().with_conn_drain(std::time::Duration::from_millis(150));
        // An upstream that never answers (issue #1616: a `wait` no longer stalls a delete).
        let upstream = slow_upstream(None).await;
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{
                "predicates": [{"equals": {"path": "/stall"}}],
                "responses": [{"proxy": {"to": format!("http://127.0.0.1:{upstream}")}}]
            }]
        }))
        .unwrap();
        let port = manager.create_imposter(config).await.expect("create");

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /stall HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(port).await.expect("delete");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "delete must return within the drain bound (~150ms), not wait out the stall; took {elapsed:?}"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "delete waited ~the drain bound before giving up (proves it attempted the drain); took {elapsed:?}"
        );
    }

    // Issue #596 (the motivating scenario): delete → immediate re-create on the SAME port must serve
    // only the new generation. Because delete unbinds the old listener before returning, the
    // re-create binds cleanly and a fresh connection can only reach the new imposter.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_then_recreate_same_port_serves_new_generation() {
        // Fixed port: re-bound after the delete (see the #809 note below).
        let port_0 = free_port();
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        // Issue #809: re-binding the *same* port after a delete is the whole point here, so the
        // port must stay fixed rather than auto-assigned. An auto-assigned port would be
        // returned to the kernel by the delete, and a concurrently-running test's auto-assign can
        // claim it before the re-create — measured at ~25% failure under `--test-threads=16` when
        // auto-assign took the lowest free port (before #1501), where the fixed port never failed.
        let cfg = |body: &str| -> ImposterConfig {
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
                "port": port_0,
                "stubs": [{
                    "predicates": [{"equals": {"path": "/g"}}],
                    "responses": [{"is": {"statusCode": 200, "body": body}}]
                }]
            }))
            .unwrap()
        };

        manager.create_imposter(cfg("old")).await.expect("create A");
        manager.delete_imposter(port_0).await.expect("delete A");
        // delete returned only after A's listener was unbound, so this binds the same port cleanly.
        manager
            .create_imposter(cfg("new"))
            .await
            .expect("recreate B");

        let mut stream = TcpStream::connect(("127.0.0.1", port_0))
            .await
            .expect("connect to re-created imposter");
        stream
            .write_all(b"GET /g HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let resp = read_until(&mut stream, "new").await;
        assert!(
            resp.contains("new") && !resp.contains("old"),
            "same-port re-create must serve the new generation, never the deleted one; got: {resp:?}"
        );
    }

    /// An imposter whose one stub answers `200 ok` after a `wait` no test ever sits through.
    fn long_wait_config(port: Option<u16>, body: &str) -> ImposterConfig {
        let mut config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": body}, "_behaviors": {"wait": 60000}}]}]
        }))
        .expect("long-wait config");
        config.port = port;
        config
    }

    // Issue #1616: deleting an imposter cuts off a request still in its `wait`, over HTTP/1: the
    // connection closes with no response bytes, as Mountebank destroys the socket. It used to hold
    // the delete for the whole 5 s drain and then answer the client from the deleted generation
    // when the wait ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_aborts_an_inflight_wait_over_http1() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        let port = manager
            .create_imposter(long_wait_config(None, "ok"))
            .await
            .expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");

        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(port).await.expect("delete");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "delete must cut the wait off, not drain it; took {elapsed:?}"
        );
        assert!(
            imposter.conn_tracker.is_empty(),
            "the waiting connection's task has ended, releasing its permit"
        );

        // Closed with no response: EOF or a reset, and never a byte of the stub's `200`.
        let mut received = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stream.read_to_end(&mut received),
        )
        .await
        .expect("the connection is closed within 1s, not left open");
        assert!(
            received.is_empty(),
            "no response bytes may be written, got {:?} ({read:?})",
            String::from_utf8_lossy(&received)
        );
    }

    // Issue #1616, over HTTP/2 prior knowledge: the `TcpFaultKind::Empty` carrier aborts the whole
    // connection beneath hyper, so the waiting stream fails instead of being answered. Every stream
    // on it belongs to the deleted imposter, so nothing else is lost.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_aborts_an_inflight_wait_over_http2() {
        use http_body_util::Empty;
        use hyper_util::rt::{TokioExecutor, TokioIo};

        let manager = ImposterManager::new();
        let port = manager
            .create_imposter(long_wait_config(None, "ok"))
            .await
            .expect("create");
        let imposter = manager.get_imposter(port).expect("imposter");

        let stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let (mut sender, conn) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .expect("h2 handshake");
        let conn = tokio::spawn(conn);
        let request = hyper::Request::get(format!("http://127.0.0.1:{port}/"))
            .body(Empty::<bytes::Bytes>::new())
            .expect("request");
        let response = tokio::spawn(sender.send_request(request));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(port).await.expect("delete");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "delete must cut the h2 wait off, not drain it; took {elapsed:?}"
        );
        assert!(
            imposter.conn_tracker.is_empty(),
            "the h2 connection task ended"
        );

        let answer = tokio::time::timeout(std::time::Duration::from_secs(1), response)
            .await
            .expect("the stream ends within 1s")
            .expect("request task");
        assert!(
            answer.is_err(),
            "the waiting h2 stream must fail, not be answered: {:?}",
            answer.map(|r| r.status())
        );
        // Probed (issue #1616): the server sends its graceful-shutdown GOAWAY, then the carrier
        // aborts the socket, so the stream fails with a closed connection (`BrokenPipe`), not a
        // per-stream RST_STREAM, and the client's connection task ends cleanly.
        let ended = tokio::time::timeout(std::time::Duration::from_secs(1), conn).await;
        assert!(ended.is_ok(), "the h2 connection is closed, not left open");
    }

    // Issue #1616: a same-port re-create after a delete that cut off a wait serves the new
    // generation, and the old waiter never gets the old generation's answer (extends #596).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn recreate_after_an_aborted_wait_serves_only_the_new_generation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        // Fixed port: re-bound after the delete (see the #809 note on the #596 test above).
        let port_0 = free_port();
        let manager = ImposterManager::new();
        manager
            .create_imposter(long_wait_config(Some(port_0), "old"))
            .await
            .expect("create old");
        let mut waiter = TcpStream::connect(("127.0.0.1", port_0))
            .await
            .expect("connect");
        waiter
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        manager.delete_imposter(port_0).await.expect("delete");
        let new: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": port_0,
            "stubs": [{"responses": [{"is": {"statusCode": 200, "body": "new"}}]}]
        }))
        .expect("new config");
        manager.create_imposter(new).await.expect("recreate");

        let mut fresh = TcpStream::connect(("127.0.0.1", port_0))
            .await
            .expect("connect to the re-created imposter");
        fresh
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write request");
        let resp = read_until(&mut fresh, "new").await;
        assert!(
            resp.contains("new"),
            "the new generation answers, got {resp:?}"
        );

        let mut old = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            waiter.read_to_end(&mut old),
        )
        .await
        .expect("the old waiter's connection is closed");
        assert!(
            old.is_empty(),
            "the deleted generation must answer nothing, got {:?}",
            String::from_utf8_lossy(&old)
        );
    }

    // =========================================================================
    // Issue #316: incremental apply_config, move_stub, and imposter change events
    // =========================================================================

    use super::super::core::StubState;
    use super::super::reconcile::{EventContext, ImposterEvent, ImposterEventListener};
    use super::super::types::RecordedRequest;
    use serde_json::json;

    fn imposter_cfg(v: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(v).expect("test imposter config")
    }

    /// `config` with its port set: a re-apply names the port the first create was assigned.
    fn at(port: u16, mut config: ImposterConfig) -> ImposterConfig {
        config.port = Some(port);
        config
    }

    // Issue #423: imposters created through the manager (the path embedded/FFI consumers use, which
    // previously got NO stub analysis) now carry computed stub-overlap warnings.
    #[tokio::test]
    async fn create_imposter_computes_stub_warnings_for_embedded() {
        use crate::extensions::stub_analysis::WarningType;
        let manager = ImposterManager::new();
        let port = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [
                    {"predicates": [{"equals": {"path": "/dup"}}],
                     "responses": [{"is": {"statusCode": 200, "body": "x"}}]},
                    {"predicates": [{"equals": {"path": "/dup"}}],
                     "responses": [{"is": {"statusCode": 200, "body": "x"}}]}
                ]
            })))
            .await
            .expect("create");
        let imposter = manager.get_imposter(port).expect("imposter exists");
        assert!(
            imposter
                .stub_warnings()
                .iter()
                .any(|w| w.warning_type == WarningType::ExactDuplicate),
            "embedded/manager-created imposter must have computed stub-analysis warnings"
        );
    }

    fn has_v6_loopback() -> bool {
        let ok = std::net::TcpListener::bind("[::1]:0").is_ok();
        if !ok {
            eprintln!("skipping: this host has no IPv6 loopback");
        }
        ok
    }

    // Issue #1137: the imposter door resolved `host` by DNS lookup, so the bracketed `[::1]` —
    // the spelling every other bind door takes — failed with a getaddrinfo error. Both the
    // auto-assign probe and an explicit port must accept it.
    #[tokio::test]
    async fn a_bracketed_ipv6_imposter_host_binds() {
        if !has_v6_loopback() {
            return;
        }
        let manager = ImposterManager::new();
        let auto = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "host": "[::1]"})))
            .await
            .expect("auto-assigned port on [::1]");
        let explicit = std::net::TcpListener::bind("[::1]:0")
            .and_then(|l| l.local_addr())
            .expect("reserve a v6 port")
            .port();
        let port = manager
            .create_imposter(imposter_cfg(
                json!({"protocol": "http", "host": "[::1]", "port": explicit}),
            ))
            .await
            .expect("explicit port on [::1]");
        assert_eq!(port, explicit);
        for p in [auto, explicit] {
            tokio::net::TcpStream::connect(("::1", p))
                .await
                .unwrap_or_else(|e| panic!("imposter on [::1]:{p} must accept: {e}"));
        }
    }

    // The bare spelling already worked here; it must keep working.
    #[tokio::test]
    async fn a_bare_ipv6_imposter_host_binds() {
        if !has_v6_loopback() {
            return;
        }
        let manager = ImposterManager::new();
        let port = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "host": "::1"})))
            .await
            .expect("auto-assigned port on ::1");
        tokio::net::TcpStream::connect(("::1", port))
            .await
            .expect("imposter on ::1 must accept");
    }

    // Mountebank hands a name to Node's `listen`, which resolves it; a literal-only imposter door
    // would be a parity regression.
    #[tokio::test]
    async fn a_named_imposter_host_still_resolves() {
        let manager = ImposterManager::new();
        manager
            .create_imposter(imposter_cfg(
                json!({"protocol": "http", "host": "localhost"}),
            ))
            .await
            .expect("a DNS name must still bind on the imposter door");
    }

    fn stub_json(body: &str) -> serde_json::Value {
        json!({
            "predicates": [{"equals": {"path": format!("/{body}")}}],
            "responses": [{"is": {"statusCode": 200, "body": body}}]
        })
    }

    fn cycled_stub_json(first: &str, second: &str) -> serde_json::Value {
        json!({
            "predicates": [{"equals": {"path": "/cycled"}}],
            "responses": [
                {"is": {"statusCode": 200, "body": first}},
                {"is": {"statusCode": 200, "body": second}}
            ]
        })
    }

    /// Serve the state's next response and return its body (advances the cycler).
    fn next_body(state: &StubState) -> String {
        let resp = state.get_next_response().expect("stub has responses");
        serde_json::to_value(resp).expect("serialize response")["is"]["body"]
            .as_str()
            .expect("string body")
            .to_string()
    }

    fn recorded(path: &str) -> RecordedRequest {
        RecordedRequest {
            body_omitted: false,
            mode: ResponseMode::Text,
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: path.to_string(),
            query: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            match_outcome: None,
            status: None,
            latency_ms: None,
            node: None,
        }
    }

    // AC1: an unchanged imposter keeps recorded requests and cycler position
    // while a sibling port is modified.
    #[tokio::test]
    async fn apply_config_preserves_untouched_imposter_state() {
        let manager = ImposterManager::new();
        let p1 = |port: Option<u16>| {
            let mut v = json!({
                "protocol": "http", "recordRequests": true,
                "stubs": [cycled_stub_json("a1", "a2"), stub_json("b"), stub_json("c")]
            });
            if let Some(port) = port {
                v["port"] = json!(port);
            }
            imposter_cfg(v)
        };
        let port_0 = manager.create_imposter(p1(None)).await.expect("create");
        let port_1 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [stub_json("x"), stub_json("y"), stub_json("z")]
            })))
            .await
            .expect("create");

        let untouched = manager.get_imposter(port_0).unwrap();
        untouched.record_request(recorded("/cycled"));
        assert_eq!(next_body(&untouched.snapshot().stubs()[0]), "a1");

        let p2_changed = imposter_cfg(json!({
            "protocol": "http", "port": port_1,
            "stubs": [stub_json("x"), stub_json("y"), stub_json("z2")]
        }));
        let report = manager
            .apply_config(vec![p1(Some(port_0)), p2_changed])
            .await
            .expect("second apply");
        assert!(report.created.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.replaced.is_empty());
        assert_eq!(report.stub_patched, vec![port_1]);

        let after = manager.get_imposter(port_0).unwrap();
        assert!(
            Arc::ptr_eq(&untouched, &after),
            "untouched imposter must not be recreated"
        );
        assert_eq!(after.get_recorded_requests().len(), 1);
        assert_eq!(
            next_body(&after.snapshot().stubs()[0]),
            "a2",
            "cycler position survives a sibling patch"
        );

        let patched = manager.get_imposter(port_1).unwrap();
        let stubs = patched.get_stubs();
        let last = serde_json::to_value(&stubs[2]).unwrap();
        assert_eq!(last["responses"][0]["is"]["body"], "z2");

        manager.delete_all().await;
    }

    // Issue #817: an enabled-only diff toggles in place — pausing must never
    // reset runtime state the way a wholesale replace would.
    #[tokio::test]
    async fn apply_config_enabled_only_diff_toggles_in_place() {
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http", "recordRequests": true,
            "stubs": [stub_json("a")]
        }));
        let port_0 = manager.create_imposter(initial).await.expect("create");
        let before = manager.get_imposter(port_0).unwrap();
        before.record_request(recorded("/a"));
        assert!(before.is_enabled());

        let paused = imposter_cfg(json!({
            "protocol": "http", "port": port_0, "recordRequests": true, "enabled": false,
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![paused]).await.expect("pause");
        assert_eq!(report.toggled, vec![port_0]);
        assert!(report.replaced.is_empty(), "never a wholesale replace");
        assert!(report.stub_patched.is_empty());

        let after = manager.get_imposter(port_0).unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "the toggle must not recreate the imposter"
        );
        assert!(!after.is_enabled());
        assert_eq!(
            after.get_recorded_requests().len(),
            1,
            "runtime state survives the pause"
        );

        // Resume via the (default, unserialized) enabled=true.
        let resumed = imposter_cfg(json!({
            "protocol": "http", "port": port_0, "recordRequests": true,
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![resumed]).await.expect("resume");
        assert_eq!(report.toggled, vec![port_0]);
        assert!(manager.get_imposter(port_0).unwrap().is_enabled());

        manager.delete_all().await;
    }

    // Issue #817: the toggle is config — it persists, emits, and a reload of
    // the persisted file restores the operator's decision.
    #[tokio::test]
    async fn set_imposter_enabled_persists_emits_and_reload_restores() {
        let datadir = tempfile::tempdir().expect("tempdir");
        let listener = Arc::new(RecordingListener(Mutex::new(Vec::new())));
        let manager = ImposterManager::with_datadir(Some(datadir.path().to_path_buf()))
            .with_event_listener(listener.clone());
        let config = imposter_cfg(json!({
            "protocol": "http", "stubs": [stub_json("a")]
        }));
        let port_0 = manager.create_imposter(config).await.expect("create");

        manager
            .set_imposter_enabled(port_0, false)
            .await
            .expect("disable");
        assert!(!manager.get_imposter(port_0).unwrap().is_enabled());
        assert!(
            listener.0.lock().iter().any(|e| matches!(
                e,
                ImposterEvent::EnabledChanged {
                    port: pat_port,
                    enabled: false
                } if *pat_port == port_0
            )),
            "the toggle must be observable"
        );

        let persisted = std::fs::read_to_string(datadir.path().join(format!("{port_0}.json")))
            .expect("persisted file");
        assert!(persisted.contains("\"enabled\": false"), "{persisted}");

        // A fresh manager loading that file restores the pause.
        let reloaded: ImposterConfig = serde_json::from_str(&persisted).expect("parses");
        assert!(!reloaded.enabled);
        let fresh = ImposterManager::new();
        // Rebind on a fresh auto-assigned port to avoid racing the old listener.
        let mut reloaded = reloaded;
        reloaded.port = None;
        let port_1 = fresh.create_imposter(reloaded).await.expect("recreate");
        assert!(
            !fresh.get_imposter(port_1).unwrap().is_enabled(),
            "a restart must not silently re-enable a paused imposter"
        );

        fresh.delete_all().await;
        manager.delete_all().await;
    }

    // Issue #817: serialization stability — the (universal) enabled case emits
    // no field, so existing configs, dumps and round-trips stay byte-identical.
    #[test]
    fn enabled_serialization_is_stable() {
        let legacy: ImposterConfig =
            serde_json::from_value(json!({ "protocol": "http", "port": 1 })).expect("parses");
        assert!(legacy.enabled, "absent field defaults to enabled");
        let value = serde_json::to_value(&legacy).expect("serialize");
        assert!(
            value.get("enabled").is_none(),
            "enabled imposters serialize no field: {value}"
        );

        let mut paused = legacy;
        paused.enabled = false;
        let value = serde_json::to_value(&paused).expect("serialize");
        assert_eq!(value["enabled"], false);
        let back: ImposterConfig = serde_json::from_value(value).expect("round-trips");
        assert!(!back.enabled);

        assert!(
            ImposterConfig::default().enabled,
            "Default must agree with the serde default"
        );
    }

    // AC2d: imposter-level field changes always replace wholesale.
    #[tokio::test]
    async fn apply_config_imposter_level_change_replaces_wholesale() {
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http", "recordRequests": true,
            "stubs": [stub_json("a")]
        }));
        let port_0 = manager.create_imposter(initial).await.expect("create");
        let before = manager.get_imposter(port_0).unwrap();
        before.record_request(recorded("/a"));

        let renamed = imposter_cfg(json!({
            "protocol": "http", "port": port_0, "recordRequests": true, "name": "renamed",
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![renamed]).await.expect("apply");
        assert_eq!(report.replaced, vec![port_0]);
        assert!(report.stub_patched.is_empty());

        let after = manager.get_imposter(port_0).unwrap();
        assert!(
            !Arc::ptr_eq(&before, &after),
            "imposter-level change recreates the imposter"
        );
        assert_eq!(after.config.name.as_deref(), Some("renamed"));
        assert!(
            after.get_recorded_requests().is_empty(),
            "wholesale replace resets runtime state"
        );

        manager.delete_all().await;
    }

    // AC2c: > 50 % of stubs changing falls back to whole-imposter replace.
    #[tokio::test]
    async fn apply_config_degenerate_stub_change_replaces_imposter() {
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http",
            "stubs": [stub_json("a"), stub_json("b")]
        }));
        let port_0 = manager.create_imposter(initial).await.expect("create");
        let before = manager.get_imposter(port_0).unwrap();

        let rewritten = imposter_cfg(json!({
            "protocol": "http", "port": port_0,
            "stubs": [stub_json("x"), stub_json("y")]
        }));
        let report = manager.apply_config(vec![rewritten]).await.expect("apply");
        assert_eq!(report.replaced, vec![port_0]);
        assert!(report.stub_patched.is_empty());
        let after = manager.get_imposter(port_0).unwrap();
        assert!(!Arc::ptr_eq(&before, &after));

        manager.delete_all().await;
    }

    // AC6: full-set validation up front — nothing mutates on validation failure.
    #[tokio::test]
    async fn apply_config_validation_failure_mutates_nothing() {
        // Neither port is ever bound: validation refuses both sets before any bind.
        let (port_1, port_2) = (1, 2);
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http",
            "stubs": [stub_json("a")]
        }));
        let port_0 = manager.create_imposter(initial).await.expect("create");
        let before = manager.get_imposter(port_0).unwrap();

        let would_change = imposter_cfg(json!({
            "protocol": "http", "port": port_0,
            "stubs": [stub_json("x")]
        }));
        let invalid = imposter_cfg(json!({
            "protocol": "tcp", "port": port_1,
            "stubs": []
        }));
        let result = manager.apply_config(vec![would_change, invalid]).await;
        assert!(
            matches!(result, Err(ImposterError::InvalidProtocol(ref p)) if p == "tcp"),
            "got: {result:?}"
        );
        let after = manager.get_imposter(port_0).unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "validation failure must not touch any imposter"
        );
        let stubs = after.get_stubs();
        assert_eq!(
            serde_json::to_value(&stubs[0]).unwrap()["responses"][0]["is"]["body"],
            "a"
        );
        assert!(manager.get_imposter(port_1).is_err());

        let dup_a = imposter_cfg(json!({"protocol": "http", "port": port_2, "stubs": []}));
        let dup_b = imposter_cfg(json!({"protocol": "http", "port": port_2, "stubs": []}));
        let result = manager.apply_config(vec![dup_a, dup_b]).await;
        assert!(matches!(result, Err(ImposterError::PortInUse(pat_port)) if pat_port == port_2));
        assert!(manager.get_imposter(port_2).is_err());

        manager.delete_all().await;
    }

    // Issue #1104: `create_imposter` auto-assigns `port: 0` (#637), but `apply_config` treated it as
    // a real port, so two of them were refused as `PortInUse(0)` on reload, `PUT /imposters` and the
    // C-ABI. A `0` must be port-less at every door.
    #[tokio::test]
    async fn apply_config_treats_port_zero_as_auto_assign() {
        let manager = ImposterManager::new();
        let set = || {
            vec![
                imposter_cfg(json!({"protocol": "http", "port": 0, "stubs": []})),
                imposter_cfg(json!({"protocol": "http", "port": 0, "stubs": []})),
                imposter_cfg(json!({"protocol": "http", "stubs": []})),
            ]
        };

        let report = manager
            .apply_config(set())
            .await
            .expect("two port-0 imposters are auto-assigned, not a duplicate port");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        let mut ports = report.created.clone();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 3, "three distinct ports: {:?}", report.created);
        assert!(ports.iter().all(|p| *p != 0), "{ports:?}");
        assert_eq!(manager.count(), 3);
        assert!(
            manager.get_imposter(0).is_err(),
            "nothing registers under 0"
        );

        // Port-less configs are never reconciled: a re-apply replaces them, it does not add more.
        manager
            .apply_config(set())
            .await
            .expect("re-applying the same set");
        assert_eq!(manager.count(), 3);

        manager.delete_all().await;
    }

    // Issue #1112: explicit ports first, and each group keeps its input order — `created` promises
    // input order, not port order, so 200 stays before 100. `port: 0` is port-less.
    #[test]
    fn explicit_ports_first_keeps_input_order_within_each_group() {
        let cfg = |port: Option<u16>, name: &str| ImposterConfig {
            port,
            name: Some(name.to_string()),
            ..Default::default()
        };
        let mut configs = vec![
            cfg(None, "auto-a"),
            cfg(Some(200), "explicit-b"),
            cfg(Some(0), "auto-b"),
            cfg(Some(100), "explicit-a"),
            cfg(None, "auto-c"),
        ];
        ImposterManager::explicit_ports_first(&mut configs);
        let names: Vec<_> = configs.iter().filter_map(|c| c.name.as_deref()).collect();
        assert_eq!(
            names,
            vec!["explicit-b", "explicit-a", "auto-a", "auto-b", "auto-c"]
        );
    }

    #[tokio::test]
    async fn apply_config_creates_new_and_deletes_missing_ports() {
        let manager = ImposterManager::new();
        let first = imposter_cfg(json!({"protocol": "http", "stubs": []}));
        let report = manager.apply_config(vec![first]).await.expect("apply");
        assert_eq!(report.created.len(), 1, "{report:?}");
        let port_0 = report.created[0];

        let second = imposter_cfg(json!({"protocol": "http", "stubs": []}));
        let report = manager.apply_config(vec![second]).await.expect("apply");
        assert_eq!(report.created.len(), 1, "{report:?}");
        let port_1 = report.created[0];
        // The delete frees `port_0` first, so auto-assign may hand the same number back.
        assert_eq!(report.deleted, vec![port_0]);
        assert_eq!(manager.count(), 1);
        assert!(manager.get_imposter(port_1).is_ok());

        manager.delete_all().await;
    }

    // Issue #1253: `apply_one` reconciles a single imposter with `apply_config`'s per-port arm.
    mod apply_one {
        use super::*;

        fn cfg(port: u16, stubs: serde_json::Value) -> ImposterConfig {
            imposter_cfg(json!({"protocol": "http", "port": port, "stubs": stubs}))
        }

        /// Create the starting imposter on an auto-assigned port, which `cfg` then names.
        async fn seed(manager: &ImposterManager, stubs: serde_json::Value) -> u16 {
            manager
                .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": stubs})))
                .await
                .expect("seed")
        }

        #[tokio::test]
        async fn creates_an_absent_port() {
            // Fixed port: `apply_one` refuses a port-less config, so the absent port is named first.
            let port_0 = free_port();
            let manager = ImposterManager::new();
            let report = manager
                .apply_one(cfg(port_0, json!([stub_json("a")])))
                .await
                .expect("apply_one");
            assert_eq!(report.created, vec![port_0]);
            assert!(report.deleted.is_empty() && report.failed.is_empty());
            assert!(manager.get_imposter(port_0).is_ok());
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn patches_in_place_and_leaves_other_ports_untouched() {
            let listener = Arc::new(RecordingListener::default());
            let manager = ImposterManager::new().with_event_listener(listener.clone());
            let stubs = |last: &str| {
                json!([
                    cycled_stub_json("p1", "p2"),
                    stub_json("b"),
                    stub_json("c"),
                    stub_json(last)
                ])
            };
            let port_0 = seed(&manager, stubs("d")).await;
            let port_1 = seed(&manager, json!([cycled_stub_json("q1", "q2")])).await;
            let p = |last: &str| cfg(port_0, stubs(last));
            let p_before = manager.get_imposter(port_0).unwrap();
            let q_before = manager.get_imposter(port_1).unwrap();
            assert_eq!(next_body(&p_before.snapshot().stubs()[0]), "p1");
            assert_eq!(next_body(&q_before.snapshot().stubs()[0]), "q1");
            listener.0.lock().clear();

            let report = manager.apply_one(p("e")).await.expect("apply_one");

            assert_eq!(report.stub_patched, vec![port_0]);
            assert!(report.created.is_empty() && report.replaced.is_empty());
            assert!(
                report.deleted.is_empty(),
                "apply_one never deletes: {report:?}"
            );
            assert!(report.failed.is_empty() && report.toggled.is_empty());
            let p_after = manager.get_imposter(port_0).unwrap();
            assert!(Arc::ptr_eq(&p_before, &p_after));
            assert_eq!(
                next_body(&p_after.snapshot().stubs()[0]),
                "p2",
                "patched imposter keeps cursors"
            );
            let q_after = manager.get_imposter(port_1).expect("other port survives");
            assert!(Arc::ptr_eq(&q_before, &q_after));
            assert_eq!(next_body(&q_after.snapshot().stubs()[0]), "q2");
            assert_eq!(
                listener.0.lock().clone(),
                vec![ImposterEvent::StubsChanged(port_0)]
            );
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn never_deletes_ports_it_was_not_given() {
            let manager = ImposterManager::new();
            let port_0 = seed(&manager, json!([stub_json("a")])).await;
            // Fixed port: `apply_one` refuses a port-less config, so the absent port is named first.
            let port_1 = free_port();
            let report = manager
                .apply_one(cfg(port_1, json!([stub_json("b")])))
                .await
                .expect("apply_one");
            assert_eq!(report.created, vec![port_1]);
            assert!(report.deleted.is_empty(), "{report:?}");
            // `ports()` is ascending; OS-assigned ports arrive in any order.
            let mut expected = vec![port_0, port_1];
            expected.sort_unstable();
            assert_eq!(manager.imposters.ports(), expected);
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn an_unchanged_config_touches_nothing() {
            let listener = Arc::new(RecordingListener::default());
            let manager = ImposterManager::new().with_event_listener(listener.clone());
            let port_0 = seed(&manager, json!([stub_json("a")])).await;
            let config = || cfg(port_0, json!([stub_json("a")]));
            let before = manager.get_imposter(port_0).unwrap();
            listener.0.lock().clear();
            let report = manager.apply_one(config()).await.expect("re-apply");
            assert_eq!(listener.0.lock().clone(), Vec::<ImposterEvent>::new());
            assert!(report.created.is_empty() && report.replaced.is_empty());
            assert!(report.stub_patched.is_empty() && report.toggled.is_empty());
            assert!(report.deleted.is_empty() && report.failed.is_empty());
            assert!(Arc::ptr_eq(&before, &manager.get_imposter(port_0).unwrap()));
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn toggles_enabled_in_place() {
            let manager = ImposterManager::new();
            let port_0 = seed(&manager, json!([stub_json("a")])).await;
            let before = manager.get_imposter(port_0).unwrap();
            let paused = imposter_cfg(json!({
                "protocol": "http", "port": port_0, "enabled": false, "stubs": [stub_json("a")]
            }));
            let report = manager.apply_one(paused).await.expect("pause");
            assert_eq!(report.toggled, vec![port_0]);
            assert!(report.replaced.is_empty());
            let after = manager.get_imposter(port_0).unwrap();
            assert!(Arc::ptr_eq(&before, &after));
            assert!(!after.is_enabled());
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn an_imposter_level_change_replaces() {
            let manager = ImposterManager::new();
            let port_0 = seed(&manager, json!([stub_json("a")])).await;
            let before = manager.get_imposter(port_0).unwrap();
            let renamed = imposter_cfg(json!({
                "protocol": "http", "port": port_0, "name": "renamed", "stubs": [stub_json("a")]
            }));
            let report = manager.apply_one(renamed).await.expect("replace");
            assert_eq!(report.replaced, vec![port_0]);
            assert!(report.deleted.is_empty() && report.failed.is_empty());
            assert!(!Arc::ptr_eq(
                &before,
                &manager.get_imposter(port_0).unwrap()
            ));
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn a_degenerate_stub_change_replaces() {
            let manager = ImposterManager::new();
            let port_0 = seed(&manager, json!([stub_json("a"), stub_json("b")])).await;
            let report = manager
                .apply_one(cfg(port_0, json!([stub_json("x"), stub_json("y")])))
                .await
                .expect("replace");
            assert_eq!(report.replaced, vec![port_0]);
            assert!(report.stub_patched.is_empty());
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn refuses_a_config_without_an_explicit_port() {
            let manager = ImposterManager::new();
            let portless = imposter_cfg(json!({"protocol": "http", "stubs": []}));
            let port_zero = imposter_cfg(json!({"protocol": "http", "port": 0, "stubs": []}));
            for config in [portless, port_zero] {
                let result = manager.apply_one(config).await;
                assert!(
                    matches!(result, Err(ImposterError::ExplicitPortRequired)),
                    "{result:?}"
                );
            }
            assert!(manager.imposters.ports().is_empty(), "nothing was created");
        }

        #[tokio::test]
        async fn refuses_duplicate_stub_ids_and_an_invalid_protocol_without_mutating() {
            let manager = ImposterManager::new();
            let port_0 = seed(&manager, json!([stub_json("a")])).await;
            let before = manager.get_imposter(port_0).unwrap();
            let mut dup_a: serde_json::Value = stub_json("a");
            dup_a["id"] = json!("same");
            let mut dup_b: serde_json::Value = stub_json("b");
            dup_b["id"] = json!("same");
            let result = manager.apply_one(cfg(port_0, json!([dup_a, dup_b]))).await;
            assert!(
                matches!(result, Err(ImposterError::StubIdConflict(ref id)) if id == "same"),
                "{result:?}"
            );
            let tcp = imposter_cfg(json!({"protocol": "tcp", "port": port_0, "stubs": []}));
            let result = manager.apply_one(tcp).await;
            assert!(
                matches!(result, Err(ImposterError::InvalidProtocol(ref p)) if p == "tcp"),
                "{result:?}"
            );
            assert!(Arc::ptr_eq(&before, &manager.get_imposter(port_0).unwrap()));
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn desired_retags_a_running_imposter_in_place() {
            let dir = tempfile::tempdir().expect("tempdir");
            let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
            let port_0 = manager
                .create_imposter_as(datadir_config(), Persistence::Ephemeral)
                .await
                .expect("create");
            let config = || datadir_cfg(json!({"port": port_0, "protocol": "http", "stubs": []}));
            let file = dir.path().join(format!("{port_0}.json"));
            let before = manager.get_imposter(port_0).unwrap();
            assert!(!file.exists());

            let report = manager
                .apply_one_desired(DesiredImposter {
                    config: config(),
                    persistence: Persistence::Datadir,
                })
                .await
                .expect("retag");
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            assert!(report.created.is_empty() && report.replaced.is_empty());
            assert!(file.exists(), "a retag to the datadir writes the file");
            assert!(Arc::ptr_eq(&before, &manager.get_imposter(port_0).unwrap()));
            manager.delete_all().await;
        }

        // `apply_one` is `apply_config`'s arm for one port: on the same starting state, the same
        // desired config yields the same report (bar `deleted`, which only the whole-set sweep fills).
        #[tokio::test]
        async fn report_matches_apply_config_for_the_same_port() {
            async fn run(via_one: bool) -> (u16, ApplyReport) {
                let manager = ImposterManager::new();
                let port = seed(
                    &manager,
                    json!([
                        cycled_stub_json("a1", "a2"),
                        stub_json("b"),
                        stub_json("c"),
                        stub_json("d")
                    ]),
                )
                .await;
                let next = imposter_cfg(json!({
                    "protocol": "http", "port": port, "enabled": false,
                    "stubs": [cycled_stub_json("a1", "a2"), stub_json("b"), stub_json("c"), stub_json("e")]
                }));
                let report = if via_one {
                    manager.apply_one(next).await
                } else {
                    manager.apply_config(vec![next]).await
                }
                .expect("apply");
                manager.delete_all().await;
                (port, report)
            }
            let (port_0, one) = run(true).await;
            let (port_1, all) = run(false).await;
            assert_eq!(one.toggled, vec![port_0]);
            assert_eq!(one.stub_patched, vec![port_0]);
            assert_eq!(all.toggled, vec![port_1]);
            assert_eq!(all.stub_patched, vec![port_1]);
            for r in [&one, &all] {
                assert!(r.created.is_empty() && r.replaced.is_empty() && r.deleted.is_empty());
                assert!(r.failed.is_empty());
            }
        }
    }

    // Issue #1256: re-applying a freshly parsed copy of the same config must not replace an
    // imposter whose stubs carry multi-key maps (predicates, response headers).
    #[tokio::test]
    async fn apply_config_identical_multi_key_set_replaces_nothing() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());
        let config = || {
            imposter_cfg(json!({
                "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"method": "GET", "path": "/m", "body": "x", "query": {"a": "1", "b": "2"}}}],
                    "responses": [
                        {"is": {"statusCode": 200, "headers": {"X-A": "1", "X-B": "2", "X-C": "3"}, "body": "m1"}},
                        {"is": {"statusCode": 200, "headers": {"X-A": "1", "X-B": "2", "X-C": "3"}, "body": "m2"}}
                    ]
                }, stub_json("b")]
            }))
        };
        let port_0 = manager.create_imposter(config()).await.expect("create");
        let before = manager.get_imposter(port_0).unwrap();
        assert_eq!(next_body(&before.snapshot().stubs()[0]), "m1");
        listener.0.lock().clear();

        for _ in 0..5 {
            let report = manager
                .apply_config(vec![at(port_0, config())])
                .await
                .expect("re-apply");
            assert!(
                report.replaced.is_empty(),
                "replaced: {:?}",
                report.replaced
            );
            assert!(
                report.stub_patched.is_empty(),
                "patched: {:?}",
                report.stub_patched
            );
            assert!(report.created.is_empty() && report.deleted.is_empty());
            assert!(report.failed.is_empty());
        }

        let after = manager.get_imposter(port_0).unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "imposter must not be recreated"
        );
        assert_eq!(
            next_body(&after.snapshot().stubs()[0]),
            "m2",
            "cycler survives"
        );
        assert_eq!(listener.0.lock().clone(), Vec::<ImposterEvent>::new());

        manager.delete_all().await;
    }

    // Issue #1254: re-applying an unchanged set leaves each imposter's stub snapshot (and so its
    // match index) as it was, rather than rebuilding an identical one per imposter per apply.
    #[tokio::test]
    async fn apply_config_identical_set_stores_no_snapshot() {
        let manager = ImposterManager::new();
        let config = || {
            imposter_cfg(json!({
                "protocol": "http",
                "stubs": [stub_json("a"), stub_json("b"), cycled_stub_json("c1", "c2")]
            }))
        };
        let port_0 = manager.create_imposter(config()).await.expect("create");
        let imposter = manager.get_imposter(port_0).unwrap();
        let before = Arc::clone(&*imposter.snapshot());

        let report = manager
            .apply_config(vec![at(port_0, config())])
            .await
            .expect("re-apply");
        assert!(report.replaced.is_empty() && report.stub_patched.is_empty());
        assert!(Arc::ptr_eq(&before, &*imposter.snapshot()));

        manager.delete_all().await;
    }

    #[derive(Default)]
    struct RecordingListener(Mutex<Vec<ImposterEvent>>);

    impl ImposterEventListener for RecordingListener {
        fn on_event(&self, event: &ImposterEvent, _ctx: &EventContext) {
            self.0.lock().push(event.clone());
        }
    }

    // AC4: events fired per mutation kind.
    #[tokio::test]
    async fn events_fired_per_mutation_kind() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());

        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [stub_json("a"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create");
        let stub_d: Stub = serde_json::from_value(stub_json("d")).unwrap();
        manager.add_stub(port_0, stub_d, None).await.expect("add");
        manager.move_stub(port_0, 0, 1).await.expect("move");
        // live stubs now [b, a, c, d]; patch one of four (below the degenerate threshold)
        // and create a sibling in the same apply.
        let patched_cfg = imposter_cfg(json!({
            "protocol": "http", "port": port_0,
            "stubs": [stub_json("b"), stub_json("a"), stub_json("c"), stub_json("e")]
        }));
        let sibling = imposter_cfg(json!({"protocol": "http", "stubs": []}));
        let report = manager
            .apply_config(vec![patched_cfg, sibling.clone()])
            .await
            .expect("apply patch+create");
        let port_1 = report.created[0];
        // drop the first imposter, keep the sibling unchanged
        manager
            .apply_config(vec![at(port_1, sibling)])
            .await
            .expect("apply delete");
        // imposter-level change on the sibling
        let renamed = imposter_cfg(json!({
            "protocol": "http", "port": port_1, "name": "renamed", "stubs": []
        }));
        manager
            .apply_config(vec![renamed])
            .await
            .expect("apply replace");
        manager.delete_imposter(port_1).await.expect("delete");
        manager.delete_all().await;

        let events = listener.0.lock().clone();
        assert_eq!(
            events,
            vec![
                ImposterEvent::Created(port_0),
                ImposterEvent::StubsChanged(port_0), // add_stub
                ImposterEvent::StubsChanged(port_0), // move_stub
                ImposterEvent::StubsChanged(port_0), // apply_config stub patch
                ImposterEvent::Created(port_1),      // apply_config create
                ImposterEvent::Deleted(port_0),      // apply_config delete
                ImposterEvent::Replaced(port_1),     // apply_config imposter-level change
                ImposterEvent::Deleted(port_1),      // delete_imposter
                ImposterEvent::AllDeleted,           // delete_all
            ]
        );
    }

    // Issue #1399: the removed imposter comes back, and the event fires once per delete.
    #[tokio::test]
    async fn delete_imposter_returning_hands_back_the_imposter_and_emits_once() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());

        let missing = 1;
        assert!(matches!(
            manager.delete_imposter_returning(missing).await,
            Err(ImposterError::NotFound(pat_port)) if pat_port == missing
        ));
        assert!(listener.0.lock().is_empty(), "a missing port emits nothing");

        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "name": "gone", "stubs": []
            })))
            .await
            .expect("create");
        let removed = manager
            .delete_imposter_returning(port_0)
            .await
            .expect("delete");
        assert_eq!(removed.config.port, Some(port_0));
        assert!(manager.get_imposter(port_0).is_err());

        let port_1 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "stubs": []
            })))
            .await
            .expect("create");
        let config = manager.delete_imposter(port_1).await.expect("delete");
        assert_eq!(config.port, Some(port_1));

        assert_eq!(
            listener.0.lock().clone(),
            vec![
                ImposterEvent::Created(port_0),
                ImposterEvent::Deleted(port_0),
                ImposterEvent::Created(port_1),
                ImposterEvent::Deleted(port_1),
            ]
        );
    }

    // ── Issue #1124: a datadir file that cannot be removed fails the delete ────────────────────────

    /// A manager whose datadir is replaced by a regular file once `setup` has persisted into it, so
    /// every later unlink fails with ENOTDIR (not NotFound), for root as well.
    struct BrokenDatadir {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
    }

    impl BrokenDatadir {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("data");
            std::fs::create_dir(&path).expect("mkdir");
            Self { _dir: dir, path }
        }

        fn break_it(&self) {
            std::fs::rename(&self.path, self.path.with_file_name("moved")).expect("move datadir");
            std::fs::write(&self.path, "not a directory").expect("replace with a file");
        }
    }

    #[tokio::test]
    async fn delete_imposter_surfaces_a_failed_unlink_and_keeps_serving() {
        let datadir = BrokenDatadir::new();
        let manager = ImposterManager::with_datadir(Some(datadir.path.clone()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create");
        datadir.break_it();

        let err = manager
            .delete_imposter(port_0)
            .await
            .expect_err("a delete whose file cannot be removed must fail");
        let message = format!("{err}");
        assert!(
            matches!(err, ImposterError::PersistError(_)),
            "got: {message}"
        );
        assert!(
            message.contains(&format!("{port_0}.json")),
            "the error names the file: {message}"
        );
        assert!(
            manager.get_imposter(port_0).is_ok(),
            "Err means not deleted: the imposter is still registered"
        );

        std::fs::remove_file(&datadir.path).expect("restore");
        std::fs::create_dir(&datadir.path).expect("restore");
        manager.delete_all().await;
    }

    #[tokio::test]
    async fn delete_all_reports_an_imposter_whose_file_cannot_be_removed() {
        let listener = Arc::new(RecordingListener::default());
        let datadir = BrokenDatadir::new();
        let manager = ImposterManager::with_datadir(Some(datadir.path.clone()))
            .with_event_listener(listener.clone());
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create persisted");
        let port_1 = manager
            .create_imposter_as(
                imposter_cfg(json!({"protocol": "http", "stubs": []})),
                Persistence::Ephemeral,
            )
            .await
            .expect("create ephemeral");
        datadir.break_it();
        listener.0.lock().clear();

        let report = manager.delete_all().await;

        assert_eq!(
            report.deleted.iter().map(|c| c.port).collect::<Vec<_>>(),
            vec![Some(port_1)],
            "the ephemeral imposter never touches the datadir, so it deletes cleanly"
        );
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, port_0);
        assert!(matches!(report.failed[0].1, ImposterError::PersistError(_)));
        assert!(manager.get_imposter(port_0).is_ok());
        assert_eq!(
            listener.0.lock().clone(),
            vec![ImposterEvent::Deleted(port_1)],
            "not everything was deleted, so AllDeleted would be false"
        );

        std::fs::remove_file(&datadir.path).expect("restore");
        std::fs::create_dir(&datadir.path).expect("restore");
        let rest = manager.delete_all().await;
        assert!(rest.failed.is_empty(), "{:?}", rest.failed);
    }

    #[tokio::test]
    async fn delete_all_emits_all_deleted_when_nothing_failed() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());
        manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create");
        listener.0.lock().clear();

        let report = manager.delete_all().await;
        assert_eq!(report.deleted.len(), 1);
        assert!(report.failed.is_empty());
        assert_eq!(listener.0.lock().clone(), vec![ImposterEvent::AllDeleted]);
    }

    #[tokio::test]
    async fn an_apply_sweep_reports_an_imposter_whose_file_cannot_be_removed() {
        let datadir = BrokenDatadir::new();
        let manager = ImposterManager::with_datadir(Some(datadir.path.clone()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create");
        datadir.break_it();

        let report = manager.apply_config(Vec::new()).await.expect("apply");
        assert!(report.deleted.is_empty(), "{report:?}");
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert_eq!(report.failed[0].0, port_0);
        assert!(manager.get_imposter(port_0).is_ok());

        std::fs::remove_file(&datadir.path).expect("restore");
        std::fs::create_dir(&datadir.path).expect("restore");
        manager.delete_all().await;
    }

    // When a create cannot be persisted, its rollback unlink fails the same way. The create must
    // still report its own write failure, not the cleanup's.
    #[tokio::test]
    async fn a_failed_create_reports_the_write_even_when_the_rollback_unlink_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("data");
        std::fs::write(&not_a_dir, "not a directory").expect("write");
        let manager = ImposterManager::with_datadir(Some(not_a_dir));

        let err = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect_err("the write fails");
        let message = format!("{err}");
        assert!(
            message.contains("Failed to write imposter "),
            "got: {message}"
        );
        assert!(!message.contains("Failed to remove"), "got: {message}");
        assert_eq!(manager.count(), 0);
    }

    // A replace whose re-create fails and whose stale-copy cleanup also fails reports both.
    #[tokio::test]
    async fn a_failed_replace_reports_a_cleanup_unlink_that_also_failed() {
        let datadir = BrokenDatadir::new();
        let manager = ImposterManager::with_datadir(Some(datadir.path.clone()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create");
        datadir.break_it();

        let report = manager
            .apply_config(vec![imposter_cfg(json!({
                "protocol": "https", "port": port_0,
                "cert": "not a pem", "key": "not a pem", "stubs": []
            }))])
            .await
            .expect("validation passes; the re-create fails");
        assert_eq!(report.deleted, vec![port_0], "{report:?}");
        let messages: Vec<String> = report.failed.iter().map(|(_, e)| format!("{e}")).collect();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(
            messages
                .iter()
                .any(|m| m.contains(&format!("Failed to remove imposter {port_0}"))),
            "the cleanup failure is reported: {messages:?}"
        );

        std::fs::remove_file(&datadir.path).expect("restore");
        std::fs::create_dir(&datadir.path).expect("restore");
        manager.delete_all().await;
    }

    // A write that runs after a delete (a stub edit that looked the imposter up first) must not
    // recreate the deleted imposter's file.
    #[tokio::test]
    async fn a_write_after_delete_does_not_recreate_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
            .await
            .expect("create");
        let stale = manager.get_imposter(port_0).expect("running");
        manager.delete_imposter(port_0).await.expect("delete");

        manager
            .persist_imposter_checked(&stale)
            .await
            .expect("a write for an imposter no longer registered is a no-op");
        assert!(!dir.path().join(format!("{port_0}.json")).exists());
    }

    #[test]
    fn remove_if_same_leaves_a_different_imposter_in_place() {
        // A table key only: nothing binds it.
        let port_0 = 1;
        let table = PortTable::new();
        let make = || {
            Arc::new(
                Imposter::new(imposter_cfg(
                    json!({"protocol": "http", "port": port_0, "stubs": []}),
                ))
                .expect("imposter"),
            )
        };
        let (first, second) = (make(), make());
        table.try_claim(port_0, Arc::clone(&second)).expect("claim");

        assert!(table.remove_if_same(port_0, &first).is_none());
        assert!(
            table.contains(port_0),
            "the imposter that took the port stays"
        );
        assert!(table.remove_if_same(port_0, &second).is_some());
        assert!(!table.contains(port_0));
        assert_eq!(table.len(), 0);
    }

    // Per-port apply failures land in `failed` while sibling ports still apply.
    #[tokio::test]
    async fn apply_config_partial_failure_reports_failed_port() {
        let manager = ImposterManager::new();
        let good = imposter_cfg(json!({"protocol": "http", "stubs": []}));
        let bad_tls = imposter_cfg(json!({
            "protocol": "https",
            "cert": "not a pem", "key": "not a pem",
            "stubs": []
        }));
        let report = manager
            .apply_config(vec![good, bad_tls])
            .await
            .expect("apply");
        assert_eq!(report.created.len(), 1, "sibling still applied: {report:?}");
        let port_0 = report.created[0];
        assert_eq!(report.failed.len(), 1);
        // A port-less config's failure is reported under the `0` sentinel.
        assert!(
            matches!(report.failed[0], (0, ImposterError::Tls(_))),
            "got: {:?}",
            report.failed
        );
        assert!(manager.get_imposter(port_0).is_ok());
        assert_eq!(manager.count(), 1);

        manager.delete_all().await;
    }

    // A create-time persist failure during apply_config lands in `failed` with no phantom entry in
    // `created` — the create rollback (issue #563) keeps create_for_apply honest under the new
    // synchronous-persist contract.
    #[tokio::test]
    async fn apply_config_create_persist_failure_reports_failed_not_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let datadir = dir.path().join("datadir");
        std::fs::create_dir(&datadir).expect("mk datadir");
        let manager = ImposterManager::with_datadir(Some(datadir.clone()));
        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let cfg = imposter_cfg(json!({"protocol": "http", "stubs": []}));
        let report = manager.apply_config(vec![cfg]).await.expect("apply");
        assert!(
            report.created.is_empty(),
            "a create that failed to persist must not appear as created: {:?}",
            report.created
        );
        assert!(
            matches!(report.failed[0], (0, ImposterError::PersistError(_))),
            "got: {:?}",
            report.failed
        );
        assert_eq!(manager.count(), 0, "must be rolled back");
    }

    // A replace whose recreate fails after teardown is honestly reported: the port lands in
    // both `deleted` and `failed`, and a Deleted event fires so listeners don't track a
    // phantom imposter.
    #[tokio::test]
    async fn apply_config_failed_replace_reports_deletion_and_event() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "stubs": [stub_json("a")]
            })))
            .await
            .expect("create");

        // Imposter-level change (protocol + TLS) whose create fails: bad PEM material.
        let bad_tls = imposter_cfg(json!({
            "protocol": "https", "port": port_0,
            "cert": "not a pem", "key": "not a pem",
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![bad_tls]).await.expect("apply");
        assert!(report.replaced.is_empty());
        assert_eq!(report.deleted, vec![port_0], "teardown really happened");
        assert!(
            matches!(report.failed[0], (pat_port, ImposterError::Tls(_)) if pat_port == port_0)
        );
        assert!(manager.get_imposter(port_0).is_err());
        assert_eq!(
            listener.0.lock().clone(),
            vec![
                ImposterEvent::Created(port_0),
                ImposterEvent::Deleted(port_0),
            ],
            "listener must learn the imposter is gone"
        );
    }

    // Duplicate explicit stub ids are rejected up front (issue #202 invariant) — they would
    // otherwise silently corrupt the stub-key diff.
    #[tokio::test]
    async fn apply_config_rejects_duplicate_stub_ids() {
        let manager = ImposterManager::new();
        let mut stub_a = stub_json("a");
        stub_a["id"] = json!("dup");
        let mut stub_b = stub_json("b");
        stub_b["id"] = json!("dup");
        let config = imposter_cfg(json!({
            "protocol": "http", "stubs": [stub_a, stub_b]
        }));
        let result = manager.apply_config(vec![config]).await;
        assert!(
            matches!(result, Err(ImposterError::StubIdConflict(ref id)) if id == "dup"),
            "got: {result:?}"
        );
        assert_eq!(manager.count(), 0, "nothing mutated");
    }

    // A datadir write failure on the patched path is observable in `failed` (issue #173),
    // while the in-memory patch stands.
    #[tokio::test]
    async fn apply_config_patch_persist_failure_lands_in_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let datadir = dir.path().join("datadir");
        std::fs::create_dir(&datadir).expect("mk datadir");
        let manager = ImposterManager::with_datadir(Some(datadir.clone()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [stub_json("a"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create succeeds with a writable datadir");
        // Break the datadir so the apply_config patch's persist fails (issue #173), while the
        // in-memory patch still stands.
        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let patched = imposter_cfg(json!({
            "protocol": "http", "port": port_0,
            "stubs": [stub_json("a"), stub_json("b"), stub_json("c2")]
        }));
        let report = manager.apply_config(vec![patched]).await.expect("apply");
        assert_eq!(report.stub_patched, vec![port_0], "in-memory patch applied");
        assert!(
            matches!(report.failed[0], (pat_port, ImposterError::PersistError(_)) if pat_port == port_0),
            "persist failure must be observable, got: {:?}",
            report.failed
        );
        let stubs = manager.get_imposter(port_0).unwrap().get_stubs();
        assert_eq!(
            serde_json::to_value(&stubs[2]).unwrap()["responses"][0]["is"]["body"],
            "c2"
        );

        manager.delete_all().await;
    }

    // Issue #1304: `toggled` means "applied in place". A toggle whose persist fails is reported
    // in `failed`; listing it in `toggled` too claims an apply that did not complete.
    #[tokio::test]
    async fn a_failed_enabled_toggle_is_reported_as_failed_not_toggled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let datadir = dir.path().join("datadir");
        std::fs::create_dir(&datadir).expect("mk datadir");
        let manager = ImposterManager::with_datadir(Some(datadir.clone()));
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "stubs": [stub_json("a")]
            })))
            .await
            .expect("create succeeds with a writable datadir");
        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let paused = imposter_cfg(json!({
            "protocol": "http", "port": port_0, "enabled": false, "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![paused]).await.expect("apply");
        assert!(
            matches!(report.failed[..], [(pat_port, ImposterError::PersistError(_))] if pat_port == port_0),
            "{:?}",
            report.failed
        );
        assert!(
            report.toggled.is_empty(),
            "a failed toggle must not be reported as toggled: {:?}",
            report.toggled
        );

        manager.delete_all().await;
    }

    // move_stub is a positional move that preserves the moved slot's cycling state.
    #[tokio::test]
    async fn move_stub_repositions_and_preserves_cursor() {
        let missing = 1;
        let manager = ImposterManager::new();
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [cycled_stub_json("a1", "a2"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create");
        let imposter = manager.get_imposter(port_0).unwrap();
        assert_eq!(next_body(&imposter.snapshot().stubs()[0]), "a1");

        manager.move_stub(port_0, 0, 2).await.expect("move");
        {
            let snap = imposter.snapshot();
            let stubs = snap.stubs();
            let first = serde_json::to_value(&stubs[0].stub).unwrap();
            assert_eq!(first["responses"][0]["is"]["body"], "b");
            assert_eq!(
                next_body(&stubs[2]),
                "a2",
                "moved stub keeps its cycling position"
            );
        }

        assert!(matches!(
            manager.move_stub(port_0, 5, 0).await,
            Err(ImposterError::StubIndexOutOfBounds(5))
        ));
        assert!(matches!(
            manager.move_stub(port_0, 0, 5).await,
            Err(ImposterError::StubIndexOutOfBounds(5))
        ));
        assert!(matches!(
            manager.move_stub(missing, 0, 0).await,
            Err(ImposterError::NotFound(pat_port)) if pat_port == missing
        ));

        manager.delete_all().await;
    }

    // =========================================================================
    // Issue #312: custom flow-store providers for embedders
    // =========================================================================
    mod flow_store_provider {
        use super::*;
        use crate::backends::InMemoryFlowStore;
        use crate::extensions::flow_state::{FlowStore, FlowStoreProvider};

        /// A provider returning a fixed store (`None` defers to the built-ins); records how
        /// many times it was consulted.
        struct FixedProvider {
            store: Option<Arc<dyn FlowStore>>,
            calls: Arc<Mutex<usize>>,
        }

        impl FlowStoreProvider for FixedProvider {
            fn provide(&self, _config: &ImposterConfig) -> Option<Arc<dyn FlowStore>> {
                *self.calls.lock() += 1;
                self.store.clone()
            }
        }

        fn gated_stub() -> serde_json::Value {
            json!({
                "scenarioName": "order",
                "requiredScenarioState": "Started",
                "newScenarioState": "paid",
                "predicates": [{"equals": {"path": "/pay"}}],
                "responses": [{"is": {"statusCode": 200}}]
            })
        }

        // AC1: a provider returning a store is used by new imposters — a write through the
        // provider's own store handle is visible via the imposter's flow store.
        #[tokio::test]
        async fn provider_store_is_used_for_new_imposters() {
            let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));
            let calls = Arc::new(Mutex::new(0));
            let provider = Arc::new(FixedProvider {
                store: Some(Arc::clone(&store)),
                calls: Arc::clone(&calls),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            let port_0 = manager
                .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
                .await
                .expect("create");

            assert_eq!(*calls.lock(), 1, "provider consulted at construction");
            // Write through the provider's store handle, read via the imposter.
            store.set("f", "k", json!("v")).expect("set");
            let imposter = manager.get_imposter(port_0).unwrap();
            assert_eq!(
                imposter.flow_get("f", "k").expect("get"),
                Some(json!("v")),
                "imposter must use the provider-supplied store"
            );

            manager.delete_all().await;
        }

        // #1518: a runtime reset leaves a provider's store alone (the embedder owns it, and a
        // recreate would be handed it back) and empties the built-in one.
        #[tokio::test]
        async fn restart_runtime_state_clears_only_the_builtin_flow_store() {
            let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));
            let provided = Arc::new(FixedProvider {
                store: Some(Arc::clone(&store)),
                calls: Arc::new(Mutex::new(0)),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provided as Arc<dyn FlowStoreProvider>);
            let port = manager
                .create_imposter(imposter_cfg(
                    json!({"protocol": "http", "stubs": [gated_stub()]}),
                ))
                .await
                .expect("create");
            let imposter = manager.get_imposter(port).expect("imposter");
            imposter.flow_set("f", "order", json!("paid")).expect("set");
            manager.restart_runtime_state(port).expect("restart");
            assert_eq!(
                imposter.flow_get("f", "order").expect("get"),
                Some(json!("paid"))
            );
            manager.delete_all().await;

            let manager = ImposterManager::new();
            let port = manager
                .create_imposter(imposter_cfg(
                    json!({"protocol": "http", "stubs": [gated_stub()]}),
                ))
                .await
                .expect("create");
            let imposter = manager.get_imposter(port).expect("imposter");
            imposter.flow_set("f", "order", json!("paid")).expect("set");
            imposter.flow_set("g", "k", json!(1)).expect("set");
            manager.restart_runtime_state(port).expect("restart");
            assert_eq!(imposter.flow_get("f", "order").expect("get"), None);
            assert_eq!(imposter.flow_get("g", "k").expect("get"), None);
            manager.delete_all().await;
        }

        // AC2: a None-returning provider falls through to the built-ins — a scenario config
        // still gets a working (non-NoOp) in-memory store.
        #[tokio::test]
        async fn none_returning_provider_falls_through_to_builtin() {
            let calls = Arc::new(Mutex::new(0));
            let provider = Arc::new(FixedProvider {
                store: None,
                calls: Arc::clone(&calls),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [gated_stub()]
                })))
                .await
                .expect("create");

            assert_eq!(*calls.lock(), 1, "provider was consulted");
            // Built-in default for scenario-declaring stubs is a real in-memory store.
            let imposter = manager.get_imposter(port_0).unwrap();
            imposter.flow_set("f", "order", json!("paid")).expect("set");
            assert_eq!(
                imposter.scenario_state("f", "order").expect("state"),
                "paid",
                "fell through to a working built-in store, not NoOp"
            );

            manager.delete_all().await;
        }

        // AC3 (regression): a manager-scoped shared provider fixes the construction-time
        // NoOp caveat — an imposter created with NO scenario stubs still advances a
        // scenario stub added later, because it got the shared real store up front.
        #[tokio::test]
        async fn late_added_scenario_stub_advances_with_provider() {
            let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));
            let provider = Arc::new(FixedProvider {
                store: Some(store),
                calls: Arc::new(Mutex::new(0)),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            // No scenario stubs, no _rift.flowState → would be NoOp without the provider.
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/plain"}}],
                        "responses": [{"is": {"statusCode": 200}}]
                    }]
                })))
                .await
                .expect("create");

            let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
            manager
                .add_stub(port_0, stub.clone(), None)
                .await
                .expect("add");

            let imposter = manager.get_imposter(port_0).unwrap();
            imposter
                .apply_scenario_transition("f", &stub)
                .expect("transition");
            assert_eq!(
                imposter.scenario_state("f", "order").expect("state"),
                "paid",
                "late-added scenario advances on the provider store (NoOp would stay Started)"
            );

            manager.delete_all().await;
        }

        // Issue #1504: with no provider, a plain imposter still gets a real default in-memory
        // store, so a scenario stub added later advances (it used to stay stuck on a no-op store).
        #[tokio::test]
        async fn no_provider_late_added_scenario_advances_on_default_store() {
            let manager = ImposterManager::new();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/plain"}}],
                        "responses": [{"is": {"statusCode": 200}}]
                    }]
                })))
                .await
                .expect("create");

            let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
            manager
                .add_stub(port_0, stub.clone(), None)
                .await
                .expect("add");

            let imposter = manager.get_imposter(port_0).unwrap();
            imposter
                .apply_scenario_transition("f", &stub)
                .expect("transition");
            assert_eq!(
                imposter.scenario_state("f", "order").expect("state"),
                "paid",
                "without a provider the default in-memory store lets a late-added scenario advance"
            );

            manager.delete_all().await;
        }

        /// Captures the config it was handed so a test can assert `provide` sees the real one.
        struct CapturingProvider {
            seen_port: Arc<Mutex<Option<u16>>>,
        }

        impl FlowStoreProvider for CapturingProvider {
            fn provide(&self, config: &ImposterConfig) -> Option<Arc<dyn FlowStore>> {
                *self.seen_port.lock() = config.port;
                Some(Arc::new(InMemoryFlowStore::new(300)))
            }
        }

        // The provider receives the real ImposterConfig (per-imposter dispatch depends on it).
        #[tokio::test]
        async fn provider_receives_the_real_config() {
            let seen_port = Arc::new(Mutex::new(None));
            let provider = Arc::new(CapturingProvider {
                seen_port: Arc::clone(&seen_port),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            let port_0 = manager
                .create_imposter(imposter_cfg(json!({"protocol": "http", "stubs": []})))
                .await
                .expect("create");

            assert_eq!(
                *seen_port.lock(),
                Some(port_0),
                "provider must receive the imposter's real config"
            );

            manager.delete_all().await;
        }

        // Precedence contract: a provider that returns Some wins even over an explicit
        // `_rift.flowState` config (a refactor reversing the order must fail here).
        #[tokio::test]
        async fn provider_wins_over_explicit_flowstate_config() {
            let store: Arc<dyn FlowStore> = Arc::new(InMemoryFlowStore::new(300));
            let provider = Arc::new(FixedProvider {
                store: Some(Arc::clone(&store)),
                calls: Arc::new(Mutex::new(0)),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "_rift": {"flowState": {"ttlSeconds": 60}},
                    "stubs": []
                })))
                .await
                .expect("create");

            // The provider's own store handle observes the imposter's writes → the provider
            // store is in use, not the built-in one the explicit flowState would have made.
            store.set("f", "k", json!("provider")).expect("set");
            let imposter = manager.get_imposter(port_0).unwrap();
            assert_eq!(
                imposter.flow_get("f", "k").expect("get"),
                Some(json!("provider")),
                "provider store must win over an explicit _rift.flowState config"
            );

            manager.delete_all().await;
        }

        // A None-returning provider with an explicit flowState config falls through to that
        // configured store (not NoOp).
        #[tokio::test]
        async fn none_provider_falls_through_to_configured_flowstate() {
            let provider = Arc::new(FixedProvider {
                store: None,
                calls: Arc::new(Mutex::new(0)),
            });
            let manager = ImposterManager::new()
                .with_flow_store_provider(provider as Arc<dyn FlowStoreProvider>);

            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "_rift": {"flowState": {"ttlSeconds": 60}},
                    "stubs": []
                })))
                .await
                .expect("create");

            let imposter = manager.get_imposter(port_0).unwrap();
            imposter.flow_set("f", "k", json!("v")).expect("set");
            assert_eq!(
                imposter.flow_get("f", "k").expect("get"),
                Some(json!("v")),
                "None provider must fall through to the configured flowState store, not NoOp"
            );

            manager.delete_all().await;
        }
    }

    // Issue #559: under concurrency, a mixed-type response array must never fall through to a
    // bogus `x-rift-no-match` 200. The old peek-classify-then-advance split let a concurrent
    // request move the cursor between the peek and the advance, so a request that classified index
    // 0 (`is`) advanced from index 1 (`proxy`), got a Proxy it wasn't handling, and served an empty
    // no-match 200. With single-advance dispatch every request serves either the `is` or a proxy
    // attempt — never the no-match marker.
    #[tokio::test]
    async fn concurrent_mixed_array_never_serves_bogus_no_match() {
        raise_fd_limit();
        let manager = ImposterManager::new();
        let port_0 = manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http",
                "stubs": [{
                    "predicates": [{"equals": {"path": "/m"}}],
                    "responses": [
                        {"is": {"statusCode": 200, "body": "OK"}},
                        {"proxy": {"to": "http://127.0.0.1:1", "mode": "proxyOnce"}}
                    ]
                }]
            })))
            .await
            .expect("create");

        // One shared client cloned into all 100 tasks (Client is Arc-internally): keeps the full
        // 100-way concurrency — that concurrency is the #559 race gate — without spawning 100
        // duplicate resolver/connection-pool stacks, which alone exhausted fds under EMFILE (#814).
        let client = reqwest::Client::new();
        let mut handles = Vec::new();
        for _ in 0..100 {
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                let resp = client
                    .get(format!("http://127.0.0.1:{port_0}/m"))
                    .send()
                    .await
                    .expect("request");
                resp.headers().contains_key("x-rift-no-match")
            }));
        }
        let mut bogus = 0;
        for h in handles {
            if h.await.expect("join") {
                bogus += 1;
            }
        }
        assert_eq!(
            bogus, 0,
            "no concurrent request may fall through to a bogus x-rift-no-match 200 (issue #559)"
        );

        manager.delete_all().await;
    }

    // =========================================================================
    // Issue #313: pluggable ResponseSequencer
    // =========================================================================
    mod response_sequencer {
        use super::*;
        use crate::behaviors::sequencer::{LocalSequencer, ResponseSequencer, SequenceKey};
        use crate::extensions::decorate::BackendUnavailable;

        type NextCall = (u16, u64, String, String, Vec<u32>);

        fn recorded(key: &SequenceKey<'_>, repeats: &[u32]) -> NextCall {
            (
                key.port,
                key.slot,
                key.stub_key.to_string(),
                key.scope.to_string(),
                repeats.to_vec(),
            )
        }

        /// Delegates to LocalSequencer while recording every next() key and reset_scope().
        #[derive(Default)]
        struct RecordingSequencer {
            inner: LocalSequencer,
            nexts: Mutex<Vec<NextCall>>,
            peeks: Mutex<Vec<NextCall>>,
            resets: Mutex<Vec<(u16, Option<String>)>>,
        }

        impl ResponseSequencer for RecordingSequencer {
            fn next(
                &self,
                key: SequenceKey<'_>,
                response_count: usize,
                repeats: &[u32],
            ) -> anyhow::Result<usize> {
                self.nexts.lock().push(recorded(&key, repeats));
                self.inner.next(key, response_count, repeats)
            }
            fn peek(
                &self,
                key: SequenceKey<'_>,
                response_count: usize,
                repeats: &[u32],
            ) -> anyhow::Result<usize> {
                self.peeks.lock().push(recorded(&key, repeats));
                self.inner.peek(key, response_count, repeats)
            }
            fn reset_scope(&self, port: u16, stub_key: Option<&str>) {
                self.resets
                    .lock()
                    .push((port, stub_key.map(str::to_string)));
                self.inner.reset_scope(port, stub_key);
            }
        }

        struct FailingSequencer;
        impl ResponseSequencer for FailingSequencer {
            fn next(&self, _key: SequenceKey<'_>, _n: usize, _r: &[u32]) -> anyhow::Result<usize> {
                Err(anyhow::Error::new(BackendUnavailable {
                    feature: "sequencer",
                    detail: "induced".to_string(),
                }))
            }
            fn peek(&self, _key: SequenceKey<'_>, _n: usize, _r: &[u32]) -> anyhow::Result<usize> {
                Err(anyhow::Error::new(BackendUnavailable {
                    feature: "sequencer",
                    detail: "induced".to_string(),
                }))
            }
            fn reset_scope(&self, _port: u16, _stub_key: Option<&str>) {}
        }

        fn manager_with_recorder() -> (Arc<RecordingSequencer>, ImposterManager) {
            let recorder = Arc::new(RecordingSequencer::default());
            let manager = ImposterManager::new()
                .with_sequencer(recorder.clone() as Arc<dyn ResponseSequencer>);
            (recorder, manager)
        }

        fn cycling_stub_json() -> serde_json::Value {
            json!({
                "id": "s1",
                "predicates": [{"equals": {"path": "/cycle"}}],
                "responses": [
                    {"is": {"statusCode": 200, "body": "one"},
                     "_behaviors": {"repeat": 2}},
                    {"is": {"statusCode": 200, "body": "two"}}
                ]
            })
        }

        fn stub_from(value: serde_json::Value) -> Stub {
            serde_json::from_value(value).expect("stub")
        }

        async fn cycle_bodies(port: u16, n: usize) -> Vec<String> {
            let mut bodies = Vec::new();
            for _ in 0..n {
                bodies.push(
                    reqwest::get(format!("http://127.0.0.1:{port}/cycle"))
                        .await
                        .expect("request")
                        .text()
                        .await
                        .expect("body"),
                );
            }
            bodies
        }

        // #1515: a replace by index restarts the stub's cycle through the sequencer too: it
        // resets the old key's scope, and the new slot starts from the first response.
        #[tokio::test]
        async fn replace_stub_by_index_resets_the_sequencer_scope() {
            let (recorder, manager) = manager_with_recorder();
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");
            assert_eq!(cycle_bodies(port, 2).await, ["one", "one"]);
            manager
                .replace_stub(port, 0, stub_from(cycling_stub_json()))
                .await
                .expect("replace");
            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(port, Some("s1".to_string()))),
                "the old key's scope is reset"
            );
            assert_eq!(cycle_bodies(port, 3).await, ["one", "one", "two"]);
            manager.delete_all().await;
        }

        // #1518: a runtime reset restarts every cursor through the sequencer, as a recreate does.
        #[tokio::test]
        async fn restart_runtime_state_resets_the_whole_sequencer_scope() {
            let (recorder, manager) = manager_with_recorder();
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");
            assert_eq!(cycle_bodies(port, 3).await, ["one", "one", "two"]);
            manager.restart_runtime_state(port).expect("restart");
            assert!(recorder.resets.lock().contains(&(port, None)));
            assert_eq!(cycle_bodies(port, 3).await, ["one", "one", "two"]);
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn replace_stub_by_id_keeps_the_sequencer_scope() {
            let (recorder, manager) = manager_with_recorder();
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");
            assert_eq!(cycle_bodies(port, 2).await, ["one", "one"]);
            manager
                .replace_stub_by_id(port, "s1", stub_from(cycling_stub_json()))
                .await
                .expect("replace");
            assert!(recorder.resets.lock().is_empty(), "no reset by id");
            assert_eq!(cycle_bodies(port, 1).await, ["two"]);
            manager.delete_all().await;
        }

        // AC2: an injected sequencer receives the correct key parts and repeats, and its
        // decisions drive the served responses (repeat honored through the real pipeline).
        #[tokio::test]
        async fn sequencer_receives_keys_and_drives_cycling() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let mut bodies = Vec::new();
            for _ in 0..3 {
                bodies.push(
                    reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                        .await
                        .expect("request")
                        .text()
                        .await
                        .expect("body"),
                );
            }
            assert_eq!(
                bodies,
                vec!["one", "one", "two"],
                "sequencer decisions must honor per-response repeats"
            );

            let nexts = recorder.nexts.lock().clone();
            assert!(!nexts.is_empty(), "sequencer was consulted");
            let (port, slot, stub_key, scope, repeats) = nexts[0].clone();
            assert_eq!(port, port_0);
            assert_eq!(stub_key, "s1", "explicit stub id is the stable key");
            assert_eq!(scope, "", "global stub has an empty scope");
            assert_eq!(repeats, vec![2, 1], "materialized per-response repeats");
            assert!(
                nexts.iter().all(|(_, s, ..)| *s == slot),
                "slot token stable across requests"
            );
            // Issue #559: response-type dispatch advances the cycler exactly once and dispatches on
            // the returned response — it no longer peeks to classify and then advances separately,
            // which is what created the TOCTOU. So the request path consults the sequencer only via
            // `next` (recorded above), never `peek`.
            let peeks = recorder.peeks.lock().clone();
            assert!(
                peeks.is_empty(),
                "dispatch must not peek the cycler — it advances once and dispatches on the result"
            );

            manager.delete_all().await;
        }

        // AC2/AC1: an in-place replace keeps the slot token (mirroring the embedded
        // preserve-on-replace), so a slot-keyed backend keeps its cursor position.
        #[tokio::test]
        async fn slot_survives_in_place_replace() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            let slot_before = recorder.nexts.lock().last().expect("recorded").1;

            let replacement: Stub = serde_json::from_value(cycling_stub_json()).expect("stub");
            manager
                .replace_stub_by_id(port_0, "s1", replacement)
                .await
                .expect("replace");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            let slot_after = recorder.nexts.lock().last().expect("recorded").1;
            assert_eq!(
                slot_before, slot_after,
                "in-place replace must keep the slot token"
            );

            manager.delete_all().await;
        }

        // AC2: reset_scope fires per stub on delete, and port-wide on bulk replace and
        // imposter teardown (the GC hook).
        #[tokio::test]
        async fn reset_scope_fires_on_delete_bulk_replace_and_teardown() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json(), stub_json("other")]
                })))
                .await
                .expect("create");

            manager
                .delete_stub_by_id(port_0, "s1")
                .await
                .expect("delete by id");
            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(port_0, Some("s1".to_string()))),
                "stub delete resets that stub's cursors: {:?}",
                recorder.resets.lock()
            );

            let fresh: Vec<Stub> = vec![serde_json::from_value(stub_json("fresh")).expect("stub")];
            manager
                .replace_stubs(port_0, fresh)
                .await
                .expect("replace all");
            assert!(
                recorder.resets.lock().contains(&(port_0, None)),
                "bulk replace resets the whole port"
            );

            recorder.resets.lock().clear();
            manager.delete_imposter(port_0).await.expect("teardown");
            assert!(
                recorder.resets.lock().contains(&(port_0, None)),
                "imposter teardown is the port-wide GC hook"
            );
        }

        // Issue #1519: clearing saved proxy responses restarts every stub's cycle, as Mountebank
        // does, so it resets the whole port even when nothing was recorded.
        #[tokio::test]
        async fn clearing_proxy_responses_resets_the_whole_port() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");
            manager.clear_proxy_responses(port_0).await.expect("clear");
            assert!(
                recorder.resets.lock().contains(&(port_0, None)),
                "{:?}",
                recorder.resets.lock()
            );
            manager.delete_all().await;
        }

        // A failing sequencer surfaces as the structured backend error (#318), never a
        // silent wrong response.
        #[tokio::test]
        async fn failing_sequencer_surfaces_structured_503() {
            let manager = ImposterManager::new()
                .with_sequencer(Arc::new(FailingSequencer) as Arc<dyn ResponseSequencer>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let resp = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            assert_eq!(resp.status(), 503, "sequencer outage is a structured 503");
            let body: serde_json::Value = resp.json().await.expect("json");
            assert_eq!(body["errors"][0]["type"], "backend unavailable");
            assert_eq!(body["errors"][0]["feature"], "sequencer");

            manager.delete_all().await;
        }

        // Scope (issue #223 space) is carried in the key: a space-scoped stub reports its
        // space, exercised directly at the imposter layer (the HTTP gate needs a matching
        // flow id, which is orthogonal here).
        #[tokio::test]
        async fn space_scoped_stub_reports_scope() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": []
                })))
                .await
                .expect("create");

            let spaced: Stub = serde_json::from_value(json!({
                "space": "flow-9",
                "predicates": [{"equals": {"path": "/sp"}}],
                "responses": [{"is": {"statusCode": 200, "body": "sp"}}]
            }))
            .expect("stub");
            manager.add_stub(port_0, spaced, None).await.expect("add");

            let imposter = manager.get_imposter(port_0).unwrap();
            let stub_state = imposter.snapshot().stubs()[0].clone();
            let _ = imposter
                .execute_stub_with_rift(&stub_state)
                .expect("sequencer ok");

            let nexts = recorder.nexts.lock().clone();
            assert_eq!(nexts.last().expect("recorded").3, "flow-9");

            manager.delete_all().await;
        }

        // A misbehaving custom sequencer returning an out-of-range index is a loud 500
        // (contract violation), never a silent fall-through to the default response.
        struct OobSequencer;
        impl ResponseSequencer for OobSequencer {
            fn next(&self, _k: SequenceKey<'_>, _n: usize, _r: &[u32]) -> anyhow::Result<usize> {
                Ok(usize::MAX)
            }
            fn peek(&self, _k: SequenceKey<'_>, _n: usize, _r: &[u32]) -> anyhow::Result<usize> {
                Ok(0)
            }
            fn reset_scope(&self, _port: u16, _stub_key: Option<&str>) {}
        }

        #[tokio::test]
        async fn out_of_range_sequencer_index_surfaces_500() {
            let manager = ImposterManager::new()
                .with_sequencer(Arc::new(OobSequencer) as Arc<dyn ResponseSequencer>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let resp = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            assert_eq!(
                resp.status(),
                500,
                "contract violation is a loud backend error"
            );

            manager.delete_all().await;
        }

        // An id-less stub keys as the stable content key (~hash#0), and delete-by-INDEX
        // resolves and resets that same key.
        #[tokio::test]
        async fn idless_stub_content_key_and_delete_by_index_reset() {
            let (recorder, manager) = manager_with_recorder();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/anon"}}],
                        "responses": [{"is": {"statusCode": 200, "body": "anon"}}]
                    }]
                })))
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/anon"))
                .await
                .expect("request");
            let key_used = recorder.nexts.lock().last().expect("recorded").2.clone();
            assert!(
                key_used.starts_with('~') && key_used.ends_with("#0"),
                "id-less stub keys as a content key, got {key_used}"
            );

            manager
                .delete_stub(port_0, 0)
                .await
                .expect("delete by index");
            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(port_0, Some(key_used.clone()))),
                "delete-by-index resets the same content key: {:?}",
                recorder.resets.lock()
            );

            manager.delete_all().await;
        }

        // apply_config patches are stub lifecycle too: a removed stub fires the per-stub
        // GC hook, and an untouched sibling keeps its slot (cursor survives the patch).
        #[tokio::test]
        async fn apply_config_patch_resets_removed_and_preserves_sibling_slot() {
            let (recorder, manager) = manager_with_recorder();
            let initial = imposter_cfg(json!({
                "protocol": "http",
                "stubs": [
                    cycling_stub_json(),
                    {"id": "doomed",
                     "predicates": [{"equals": {"path": "/doomed"}}],
                     "responses": [{"is": {"statusCode": 200, "body": "bye"}}]}
                ]
            }));
            let port_0 = manager
                .create_imposter(initial.clone())
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            let slot_before = recorder.nexts.lock().last().expect("recorded").1;

            let mut patched = at(port_0, initial);
            patched.stubs.truncate(1);
            manager.apply_config(vec![patched]).await.expect("patch");

            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(port_0, Some("doomed".to_string()))),
                "apply_config removal fires the per-stub GC hook: {:?}",
                recorder.resets.lock()
            );

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/cycle"))
                .await
                .expect("request");
            let slot_after = recorder.nexts.lock().last().expect("recorded").1;
            assert_eq!(
                slot_before, slot_after,
                "an untouched sibling keeps its slot across an apply_config patch"
            );

            manager.delete_all().await;
        }
    }

    // =========================================================================
    // Issue #314: pluggable RequestJournal
    // =========================================================================
    mod request_journal {
        use super::*;
        use crate::imposter::journal::{JournalRead, LocalJournal, RequestJournal};
        use crate::imposter::types::RecordedRequest;

        /// Delegates to LocalJournal while recording every trait call.
        #[derive(Default)]
        struct RecordingJournal {
            inner: LocalJournal,
            notes: Mutex<Vec<u16>>,
            records: Mutex<Vec<(u16, String, String)>>,
            clears: Mutex<Vec<u16>>,
            flow_clears: Mutex<Vec<(u16, String)>>,
            retains: Mutex<Vec<u16>>,
        }

        impl RequestJournal for RecordingJournal {
            fn note_request(&self, port: u16) {
                self.notes.lock().push(port);
                self.inner.note_request(port);
            }
            fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
                self.records
                    .lock()
                    .push((port, flow_id.to_string(), req.path.clone()));
                self.inner.record(port, flow_id, req);
            }
            fn read(&self, port: u16) -> JournalRead {
                self.inner.read(port)
            }
            fn clear(&self, port: u16) -> anyhow::Result<()> {
                self.clears.lock().push(port);
                self.inner.clear(port)
            }
            fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
                self.retains.lock().push(port);
                self.inner.retain(port, keep);
            }
            fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
                self.flow_clears.lock().push((port, flow_id.to_string()));
                self.inner.clear_flow(port, flow_id)
            }
            fn count(&self, port: u16) -> u64 {
                self.inner.count(port)
            }
        }

        /// A journal whose reads are flagged incomplete (backend partially unreachable).
        struct IncompleteJournal(LocalJournal);
        impl RequestJournal for IncompleteJournal {
            fn note_request(&self, port: u16) {
                self.0.note_request(port);
            }
            fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
                self.0.record(port, flow_id, req);
            }
            fn read(&self, port: u16) -> JournalRead {
                JournalRead {
                    complete: false,
                    ..self.0.read(port)
                }
            }
            fn clear(&self, port: u16) -> anyhow::Result<()> {
                self.0.clear(port)
            }
            fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
                self.0.retain(port, keep);
            }
            fn clear_flow(&self, port: u16, flow_id: &str) -> anyhow::Result<()> {
                self.0.clear_flow(port, flow_id)
            }
            fn count(&self, port: u16) -> u64 {
                self.0.count(port)
            }
        }

        fn manager_with_journal() -> (Arc<RecordingJournal>, ImposterManager) {
            let journal = Arc::new(RecordingJournal::default());
            let manager = ImposterManager::new()
                .with_request_journal(journal.clone() as Arc<dyn RequestJournal>);
            (journal, manager)
        }

        // AC2: note_request fires for EVERY request even with recording off, backing
        // numberOfRequests; nothing is recorded.
        #[tokio::test]
        async fn note_request_counts_even_when_recording_off() {
            let (journal, manager) = manager_with_journal();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": false,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/x"))
                .await
                .expect("request");
            assert_eq!(*journal.notes.lock(), vec![port_0]);
            assert!(journal.records.lock().is_empty(), "recording is off");
            let imposter = manager.get_imposter(port_0).unwrap();
            assert_eq!(
                imposter.get_request_count(),
                1,
                "numberOfRequests backed by journal"
            );

            manager.delete_all().await;
        }

        // AC2: record carries the request's resolved flow id (per flowIdSource).
        #[tokio::test]
        async fn record_carries_resolved_flow_id() {
            let (journal, manager) = manager_with_journal();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "_rift": { "flowState": { "flowIdSource": "header:X-Flow-Id" } },
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let client = reqwest::Client::new();
            let _ = client
                .get(format!("http://127.0.0.1:{port_0}/x"))
                .header("X-Flow-Id", "flow-42")
                .send()
                .await
                .expect("request");
            let records = journal.records.lock().clone();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, port_0);
            assert_eq!(records[0].1, "flow-42", "flow id resolved per flowIdSource");

            // No header → falls back to the imposter port, matching resolve semantics.
            let _ = client
                .get(format!("http://127.0.0.1:{port_0}/y"))
                .send()
                .await
                .expect("request");
            assert_eq!(
                journal.records.lock().last().expect("recorded").1,
                format!("{port_0}")
            );

            manager.delete_all().await;
        }

        // AC2: admin-facing imposter methods route through the injected journal.
        #[tokio::test]
        async fn admin_paths_route_through_journal() {
            let (journal, manager) = manager_with_journal();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/seen"))
                .await
                .expect("request");
            let imposter = manager.get_imposter(port_0).unwrap();
            let recorded = imposter.get_recorded_requests();
            assert_eq!(recorded.len(), 1, "GET requests reads via the journal");
            assert_eq!(recorded[0].path, "/seen");

            imposter.retain_recorded_requests(|r| r.path != "/seen");
            assert_eq!(*journal.retains.lock(), vec![port_0]);
            assert!(imposter.get_recorded_requests().is_empty());
            assert_eq!(imposter.get_request_count(), 1, "retain keeps the count");

            imposter.clear_recorded_requests().expect("clear");
            assert!(journal.clears.lock().contains(&port_0));
            assert_eq!(imposter.get_request_count(), 0, "clear resets the count");

            manager.delete_all().await;
        }

        // AC1/AC2: teardown_space clears exactly one correlated slice via clear_flow.
        #[tokio::test]
        async fn teardown_space_uses_clear_flow() {
            let (journal, manager) = manager_with_journal();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "_rift": { "flowState": { "flowIdSource": "header:X-Flow-Id" } },
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let client = reqwest::Client::new();
            for flow in ["sp-1", "sp-2"] {
                let _ = client
                    .get(format!("http://127.0.0.1:{port_0}/x"))
                    .header("X-Flow-Id", flow)
                    .send()
                    .await
                    .expect("request");
            }

            let imposter = manager.get_imposter(port_0).unwrap();
            imposter.teardown_space("sp-1").expect("teardown");
            assert!(
                journal
                    .flow_clears
                    .lock()
                    .contains(&(port_0, "sp-1".to_string())),
                "teardown routes through clear_flow: {:?}",
                journal.flow_clears.lock()
            );
            let remaining = imposter.get_recorded_requests();
            assert_eq!(remaining.len(), 1, "only the torn-down slice is dropped");

            manager.delete_all().await;
        }

        // Imposter deletion is the port-wide GC hook for a shared backend: stale entries
        // must not resurrect on a later imposter reusing the port.
        #[tokio::test]
        async fn delete_imposter_clears_the_port() {
            let (journal, manager) = manager_with_journal();
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/x"))
                .await
                .expect("request");

            manager.delete_imposter(port_0).await.expect("delete");
            assert!(journal.clears.lock().contains(&port_0));
            assert_eq!(journal.inner.count(port_0), 0);
            assert!(journal.inner.read(port_0).entries.is_empty());
        }

        // Two live imposters sharing one injected journal stay isolated through the
        // imposter wrappers: clear on port A leaves port B's entries and count intact.
        #[tokio::test]
        async fn shared_journal_isolates_ports_through_wrappers() {
            let (_journal, manager) = manager_with_journal();
            let mut ports = Vec::new();
            for _ in 0..2 {
                let port = manager
                    .create_imposter(imposter_cfg(json!({
                        "protocol": "http", "recordRequests": true,
                        "stubs": [stub_json("ok")]
                    })))
                    .await
                    .expect("create");
                let _ = reqwest::get(format!("http://127.0.0.1:{port}/x"))
                    .await
                    .expect("request");
                ports.push(port);
            }
            let (port_1, port_0) = (ports[0], ports[1]);

            let a = manager.get_imposter(port_1).unwrap();
            let b = manager.get_imposter(port_0).unwrap();
            a.clear_recorded_requests().expect("clear");

            assert!(a.get_recorded_requests().is_empty());
            assert_eq!(a.get_request_count(), 0);
            assert_eq!(b.get_recorded_requests().len(), 1, "sibling port untouched");
            assert_eq!(b.get_request_count(), 1);

            manager.delete_all().await;
        }

        // An incomplete read (backend partially unreachable) still serves what it got.
        #[tokio::test]
        async fn incomplete_read_still_serves_entries() {
            let manager = ImposterManager::new()
                .with_request_journal(
                    Arc::new(IncompleteJournal(LocalJournal::default())) as Arc<dyn RequestJournal>
                );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/x"))
                .await
                .expect("request");
            let imposter = manager.get_imposter(port_0).unwrap();
            assert_eq!(
                imposter.get_recorded_requests().len(),
                1,
                "partial data is served, not dropped"
            );
            let read = imposter.read_recorded_requests();
            assert!(
                !read.complete && read.entries.len() == 1,
                "the completeness flag is observable at the core API"
            );

            manager.delete_all().await;
        }

        /// A journal whose two clear operations fail like an unreachable remote backend
        /// (issue #330); everything else delegates to a working LocalJournal so imposters
        /// still record and read normally.
        #[derive(Default)]
        struct FailingClearJournal(LocalJournal);
        impl FailingClearJournal {
            fn unavailable() -> anyhow::Error {
                anyhow::Error::new(crate::extensions::decorate::BackendUnavailable {
                    feature: "requestJournal",
                    detail: "clear failed".to_string(),
                })
            }
        }
        impl RequestJournal for FailingClearJournal {
            fn note_request(&self, port: u16) {
                self.0.note_request(port);
            }
            fn record(&self, port: u16, flow_id: &str, req: RecordedRequest) {
                self.0.record(port, flow_id, req);
            }
            fn read(&self, port: u16) -> JournalRead {
                self.0.read(port)
            }
            fn clear(&self, _port: u16) -> anyhow::Result<()> {
                Err(Self::unavailable())
            }
            fn retain(&self, port: u16, keep: &dyn Fn(&RecordedRequest) -> bool) {
                self.0.retain(port, keep);
            }
            fn clear_flow(&self, _port: u16, _flow_id: &str) -> anyhow::Result<()> {
                Err(Self::unavailable())
            }
            fn count(&self, port: u16) -> u64 {
                self.0.count(port)
            }
        }

        // AC5 (#330): a failed backend clear propagates out of clear_recorded_requests
        // instead of being reported as a clean clear.
        #[tokio::test]
        async fn clear_recorded_requests_propagates_error() {
            let manager = ImposterManager::new()
                .with_request_journal(
                    Arc::new(FailingClearJournal::default()) as Arc<dyn RequestJournal>
                );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let imposter = manager.get_imposter(port_0).unwrap();
            let err = imposter
                .clear_recorded_requests()
                .expect_err("a failed backend clear must surface");
            assert!(
                err.downcast_ref::<crate::extensions::decorate::BackendUnavailable>()
                    .is_some(),
                "the backend error is preserved for 503 mapping"
            );

            manager.delete_all().await;
        }

        // AC2 (#330): teardown_space folds a clear_flow failure into its first-error report.
        #[tokio::test]
        async fn teardown_space_surfaces_journal_clear_failure() {
            let manager = ImposterManager::new()
                .with_request_journal(
                    Arc::new(FailingClearJournal::default()) as Arc<dyn RequestJournal>
                );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let imposter = manager.get_imposter(port_0).unwrap();
            let err = imposter
                .teardown_space("sp-1")
                .expect_err("a failed scoped clear must surface, not report a clean teardown");
            assert!(
                err.downcast_ref::<crate::extensions::decorate::BackendUnavailable>()
                    .is_some(),
                "the backend error is preserved"
            );

            manager.delete_all().await;
        }

        // AC4 (#330): the delete-time GC clear is best-effort — a failed clear is logged but
        // must not fail the delete.
        #[tokio::test]
        async fn delete_imposter_survives_journal_clear_failure() {
            let manager = ImposterManager::new()
                .with_request_journal(
                    Arc::new(FailingClearJournal::default()) as Arc<dyn RequestJournal>
                );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            manager
                .delete_imposter(port_0)
                .await
                .expect("delete succeeds despite a failed GC clear");
        }
    }

    // =========================================================================
    // Issue #315: pluggable ProxyRecordingStore
    // =========================================================================
    mod proxy_store {
        use super::*;
        use crate::extensions::decorate::BackendUnavailable;
        use crate::recording::{
            ClaimOutcome, ClaimToken, LocalProxyStore, ProxyMode, ProxyRecordingStore,
            ProxyStoreError, RecordedResponse, RequestSignature, StubPlacement, StubPublication,
        };

        /// Delegates to a LocalProxyStore while recording every claim/release/clear it sees.
        /// `fail_claim`/`fail_record` inject the `Err` paths a real external backend can take,
        /// which the built-in store never exercises.
        struct SpyProxyStore {
            inner: LocalProxyStore,
            fail_claim: bool,
            /// `try_claim` answers `Refused` — the exactly-once arbiter could not decide, so the
            /// engine must fail the request rather than forward it.
            refuse_claim: bool,
            fail_record: bool,
            claims: Mutex<Vec<u16>>,
            releases: Mutex<Vec<u16>>,
            clears: Mutex<Vec<u16>>,
        }

        impl SpyProxyStore {
            fn new() -> Self {
                Self::with_faults(false, false)
            }

            fn with_faults(fail_claim: bool, fail_record: bool) -> Self {
                Self {
                    refuse_claim: false,
                    ..Self::build(fail_claim, fail_record)
                }
            }

            /// The arbiter-cannot-answer path: `try_claim` returns
            /// [`ProxyStoreError::Refused`], so the engine must fail the request.
            fn refusing() -> Self {
                Self {
                    refuse_claim: true,
                    ..Self::build(false, false)
                }
            }

            fn build(fail_claim: bool, fail_record: bool) -> Self {
                Self {
                    inner: LocalProxyStore::new(ProxyMode::ProxyOnce),
                    fail_claim,
                    refuse_claim: false,
                    fail_record,
                    claims: Mutex::new(Vec::new()),
                    releases: Mutex::new(Vec::new()),
                    clears: Mutex::new(Vec::new()),
                }
            }
        }

        impl ProxyRecordingStore for SpyProxyStore {
            fn try_claim(
                &self,
                port: u16,
                sig: &RequestSignature,
            ) -> std::result::Result<ClaimOutcome, ProxyStoreError> {
                self.try_claim_as(port, sig, self.inner.mode())
            }
            fn try_claim_as(
                &self,
                port: u16,
                sig: &RequestSignature,
                mode: ProxyMode,
            ) -> std::result::Result<ClaimOutcome, ProxyStoreError> {
                self.claims.lock().push(port);
                if self.refuse_claim {
                    return Err(ProxyStoreError::Refused(BackendUnavailable {
                        feature: "spy",
                        detail: "spy refuses to arbitrate".to_owned(),
                    }));
                }
                if self.fail_claim {
                    return Err(ProxyStoreError::Unavailable("spy".into()));
                }
                self.inner.try_claim_as(port, sig, mode)
            }
            fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
                self.releases.lock().push(port);
                self.inner.release_claim(port, sig, token);
            }
            fn record(
                &self,
                port: u16,
                sig: RequestSignature,
                token: ClaimToken,
                resp: RecordedResponse,
            ) -> std::result::Result<(), ProxyStoreError> {
                if self.fail_record {
                    // Simulate a backend that fails WITHOUT self-clearing its claim, so the
                    // caller's release-on-error is what keeps the signature retryable.
                    return Err(ProxyStoreError::Unavailable("spy".into()));
                }
                self.inner.record(port, sig, token, resp)
            }
            fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
                self.inner.lookup(port, sig)
            }
            fn clear(&self, port: u16) {
                self.clears.lock().push(port);
                self.inner.clear(port);
            }
        }

        /// Create an "UP" upstream on an OS-assigned port; returns the bound port. Auto-assign
        /// (not a fixed literal) so concurrently-running tests can never share a listener — the
        /// fixed-port + SO_REUSEPORT collision behind issue #587.
        async fn upstream(manager: &ImposterManager) -> u16 {
            let cfg = imposter_cfg(json!({
                "protocol": "http",
                "stubs": [{ "responses": [{ "is": { "statusCode": 200, "body": "UP" } }] }]
            }));
            manager.create_imposter(cfg).await.expect("create upstream")
        }

        /// A port with nothing listening, chosen so the suite cannot race it (issue #859). Port 1
        /// is below the privileged threshold, so no test can bind it, and outside the ephemeral
        /// range `bind(:0)` allocates from, so the allocator can never hand it out — `connect`
        /// here is always refused. Binding an ephemeral port and dropping the listener, as this
        /// did before, returns the number to the allocator immediately; under full-suite
        /// parallelism another test took it before the proxy dialled, the upstream *succeeded*,
        /// and no claim was released.
        const CLOSED_PORT: u16 = 1;

        // AC7: a shared proxy store is injected into imposters, keyed by port, exercised on the
        // proxy hot path, and cleared on imposter deletion.
        #[tokio::test]
        async fn shared_store_is_used_and_cleared_on_delete() {
            raise_fd_limit();
            let spy = Arc::new(SpyProxyStore::new());
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);

            let up = upstream(&manager).await;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{up}")).await;

            // The proxy imposter shares the injected store, not its private default.
            let imposter = manager.get_imposter(proxy_port).unwrap();
            assert!(Arc::ptr_eq(
                &imposter.proxy_store,
                &(spy.clone() as Arc<dyn ProxyRecordingStore>)
            ));

            // Driving the proxy leg fires the shared store's claim, keyed by the imposter port.
            let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/thing"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body");
            assert_eq!(body, "UP");
            assert!(
                spy.claims.lock().contains(&proxy_port),
                "shared store claimed on the imposter port"
            );

            // Deleting the imposter reclaims the shared store's port slice.
            manager.delete_imposter(proxy_port).await.expect("delete");
            assert!(
                spy.clears.lock().contains(&proxy_port),
                "delete clears the port's saved recordings"
            );

            manager.delete_all().await;
        }

        /// Build a proxyOnce imposter forwarding to `to` on an OS-assigned port; returns the port.
        async fn proxy_imposter(manager: &ImposterManager, to: &str) -> u16 {
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{ "responses": [{ "proxy": { "to": to, "mode": "proxyOnce" } }] }]
                })))
                .await
                .expect("create proxy imposter")
        }

        // AC2 end-to-end: a failed upstream call releases the claim through
        // handle_proxy_request, so an identical retry can claim again instead of wedging.
        // Two identical failing requests each release → the signature never gets stuck InFlight.
        #[tokio::test]
        async fn upstream_failure_releases_claim_end_to_end() {
            raise_fd_limit();
            let spy = Arc::new(SpyProxyStore::new());
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            // Forward to a closed port so the upstream leg always fails.
            let dead = CLOSED_PORT;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{dead}")).await;

            for _ in 0..2 {
                let _ = reqwest::get(format!("http://127.0.0.1:{proxy_port}/wedge")).await;
            }

            assert_eq!(
                spy.releases.lock().len(),
                2,
                "each failed upstream call releases its claim; the signature stays reclaimable"
            );

            manager.delete_all().await;
        }

        // The record()-failure path (issue #315, review finding): a backend that returns Err
        // from record without self-clearing must have its claim released by the caller, or the
        // signature wedges. Two successful upstream calls whose record fails must each release.
        #[tokio::test]
        async fn record_failure_releases_claim_end_to_end() {
            raise_fd_limit();
            let spy = Arc::new(SpyProxyStore::with_faults(false, true));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{up}")).await;

            for _ in 0..2 {
                let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/rec"))
                    .await
                    .expect("request")
                    .text()
                    .await
                    .expect("body");
                assert_eq!(body, "UP", "client still gets the upstream response");
            }

            assert_eq!(
                spy.releases.lock().len(),
                2,
                "a failed record releases the claim, so the second request can claim again"
            );

            manager.delete_all().await;
        }

        /// The refusal path (rift-cluster#529): when `try_claim` answers `Refused`, the store is
        /// the exactly-once arbiter and could not decide, so the engine must **fail the request**
        /// and never call the upstream. Forwarding here would mean a real upstream call for every
        /// request the outage lasts — the unbounded duplicate `proxyOnce` exists to prevent.
        ///
        /// The zero-hit assertion is the load-bearing half: a 503 alone would also be produced by
        /// an implementation that forwarded first and failed afterwards.
        #[tokio::test]
        async fn store_refusal_answers_503_and_never_forwards() {
            raise_fd_limit();
            let spy = Arc::new(SpyProxyStore::refusing());
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{up}")).await;

            let response = reqwest::get(format!("http://127.0.0.1:{proxy_port}/refused"))
                .await
                .expect("request");
            assert_eq!(
                response.status(),
                503,
                "a refused claim answers 503, not the upstream's response"
            );
            let body = response.text().await.expect("body");
            assert!(
                body.contains(crate::response::ErrorKind::BackendUnavailable.slug())
                    && body.contains("spy"),
                "the #318 envelope must name the backend that refused: {body}"
            );

            assert_eq!(
                manager
                    .get_imposter(up)
                    .expect("upstream imposter")
                    .get_request_count(),
                0,
                "the upstream must never be called for a refused claim — that duplicate call is \
                 the whole reason the refusal exists"
            );
            assert!(
                spy.releases.lock().is_empty(),
                "no claim was granted, so nothing to release"
            );

            manager.delete_all().await;
        }

        // ProxyStoreError degrade path: when try_claim returns Err (backend unavailable), the
        // imposter still forwards upstream successfully — it just doesn't record.
        #[tokio::test]
        async fn store_unavailable_still_forwards() {
            let spy = Arc::new(SpyProxyStore::with_faults(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{up}")).await;

            let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/degrade"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body");
            assert_eq!(
                body, "UP",
                "forwards upstream despite the store being unavailable"
            );
            assert!(
                spy.releases.lock().is_empty(),
                "no claim was granted, so nothing to release"
            );

            manager.delete_all().await;
        }

        // =====================================================================
        // Issue #910: complete-after-stub + store-owned stub publication
        // =====================================================================

        /// What `complete` saw: port, placement, `proxy_to`, the stub and the anchor index.
        type Completion = (u16, StubPlacement, String, Stub, Option<usize>);

        /// A store that settles claims through `complete` and can own stub publication.
        /// `publishes` drives `publishes_stubs()`; `fail_complete` injects the publication-failed
        /// path a distributed backend takes when it cannot commit the stub.
        struct PublishingProxyStore {
            inner: LocalProxyStore,
            publishes: bool,
            fail_complete: bool,
            /// `(port, placement, proxy_to, stub)` for every completion the engine handed over.
            completions: Mutex<Vec<Completion>>,
            /// Recordings settled through the pre-#910 `record` path (no stub was generated).
            records: Mutex<Vec<u16>>,
            releases: Mutex<Vec<u16>>,
        }

        impl PublishingProxyStore {
            fn new(publishes: bool, fail_complete: bool) -> Self {
                Self {
                    inner: LocalProxyStore::new(ProxyMode::ProxyOnce),
                    publishes,
                    fail_complete,
                    completions: Mutex::new(Vec::new()),
                    records: Mutex::new(Vec::new()),
                    releases: Mutex::new(Vec::new()),
                }
            }
        }

        impl ProxyRecordingStore for PublishingProxyStore {
            fn try_claim(
                &self,
                port: u16,
                sig: &RequestSignature,
            ) -> std::result::Result<ClaimOutcome, ProxyStoreError> {
                self.inner.try_claim(port, sig)
            }
            fn try_claim_as(
                &self,
                port: u16,
                sig: &RequestSignature,
                mode: ProxyMode,
            ) -> std::result::Result<ClaimOutcome, ProxyStoreError> {
                self.inner.try_claim_as(port, sig, mode)
            }
            fn release_claim(&self, port: u16, sig: &RequestSignature, token: ClaimToken) {
                self.releases.lock().push(port);
                self.inner.release_claim(port, sig, token);
            }
            fn record(
                &self,
                port: u16,
                sig: RequestSignature,
                token: ClaimToken,
                resp: RecordedResponse,
            ) -> std::result::Result<(), ProxyStoreError> {
                self.records.lock().push(port);
                self.inner.record(port, sig, token, resp)
            }
            fn complete(
                &self,
                port: u16,
                sig: RequestSignature,
                token: ClaimToken,
                resp: RecordedResponse,
                publication: &StubPublication<'_>,
            ) -> std::result::Result<(), ProxyStoreError> {
                self.completions.lock().push((
                    port,
                    publication.placement,
                    publication.proxy_to.to_string(),
                    publication.stub.clone(),
                    publication.anchor_index,
                ));
                if self.fail_complete {
                    return Err(ProxyStoreError::Unavailable("publication failed".into()));
                }
                self.inner.record(port, sig, token, resp)
            }
            fn publishes_stubs(&self) -> bool {
                self.publishes
            }
            fn lookup(&self, port: u16, sig: &RequestSignature) -> Option<RecordedResponse> {
                self.inner.lookup(port, sig)
            }
            fn clear(&self, port: u16) {
                self.inner.clear(port);
            }
        }

        /// A proxy imposter with `predicateGenerators`, so the engine builds a stub from the
        /// proxied response and therefore reaches the publication seam. Returns the port.
        async fn generating_proxy_imposter(manager: &ImposterManager, to: &str, mode: &str) -> u16 {
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{ "responses": [{ "proxy": {
                        "to": to,
                        "mode": mode,
                        "predicateGenerators": [{ "matches": { "path": true } }]
                    } }] }]
                })))
                .await
                .expect("create generating proxy imposter")
        }

        // AC3/AC4: with `publishes_stubs() == true` the engine performs no stub insertion of its
        // own, and the store receives the built stub plus the resolved insertion intent.
        #[tokio::test]
        async fn publishing_store_owns_stub_insertion() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let to = format!("http://127.0.0.1:{up}");
            let proxy_port = generating_proxy_imposter(&manager, &to, "proxyOnce").await;

            let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/owned"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body");
            assert_eq!(body, "UP");

            {
                let completions = spy.completions.lock();
                assert_eq!(
                    completions.len(),
                    1,
                    "the claim is settled through complete(), not record()"
                );
                assert_eq!(completions[0].0, proxy_port);
                assert_eq!(
                    completions[0].1,
                    StubPlacement::BeforeProxy,
                    "proxyOnce recordings belong before the proxy stub"
                );
                assert_eq!(
                    completions[0].2, to,
                    "the anchor the placement is relative to"
                );
                assert!(
                    !completions[0].3.predicates.is_empty(),
                    "the store receives the fully-built stub, generated predicates included"
                );
            }
            assert!(
                spy.records.lock().is_empty(),
                "a generated stub settles via complete(), never via record()"
            );

            assert_eq!(
                manager.get_imposter(proxy_port).unwrap().stub_count(),
                1,
                "the store owns publication; the engine inserted nothing locally"
            );

            manager.delete_all().await;
        }

        // AC1: the default (`publishes_stubs() == false`) keeps the engine's own insertion, so a
        // non-publishing store sees exactly today's behaviour.
        #[tokio::test]
        async fn non_publishing_store_keeps_engine_insertion() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(false, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let to = format!("http://127.0.0.1:{up}");
            let proxy_port = generating_proxy_imposter(&manager, &to, "proxyOnce").await;

            let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/kept"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body");
            assert_eq!(body, "UP");

            assert_eq!(spy.completions.lock().len(), 1);
            assert_eq!(
                manager.get_imposter(proxy_port).unwrap().stub_count(),
                2,
                "the engine still inserts the recorded stub next to the proxy stub"
            );

            manager.delete_all().await;
        }

        // Issue #1389: a publisher learns which stub answered, by index, so it can place the
        // recording by that stub rather than by the first proxy to the same upstream.
        #[tokio::test]
        async fn completion_names_the_index_of_the_stub_that_answered() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let to = format!("http://127.0.0.1:{up}");
            let proxy = |path: &str| {
                json!({
                    "predicates": [{ "equals": { "path": path } }],
                    "responses": [{ "proxy": {
                        "to": to,
                        "mode": "proxyOnce",
                        "predicateGenerators": [{ "matches": { "path": true } }]
                    } }]
                })
            };
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [proxy("/first"), { "responses": [{ "is": { "body": "x" } }],
                              "predicates": [{ "equals": { "path": "/other" } }] }, proxy("/third")]
                })))
                .await
                .expect("create imposter");

            let _ = reqwest::get(format!("http://127.0.0.1:{port}/third")).await;

            {
                let completions = spy.completions.lock();
                assert_eq!(completions.len(), 1);
                assert_eq!(
                    completions[0].4,
                    Some(2),
                    "the third stub answered, not the first proxy to the same upstream"
                );
            }
            manager.delete_all().await;
        }

        // AC4: proxyAlways resolves to the after-the-proxy-stub, merge-on-equal-predicates intent,
        // so a publisher never has to re-derive it from the mode string.
        #[tokio::test]
        async fn proxy_always_completion_carries_after_proxy_placement() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let to = format!("http://127.0.0.1:{up}");
            let proxy_port = generating_proxy_imposter(&manager, &to, "proxyAlways").await;

            let _ = reqwest::get(format!("http://127.0.0.1:{proxy_port}/always")).await;

            {
                let completions = spy.completions.lock();
                assert_eq!(completions.len(), 1);
                assert_eq!(
                    completions[0].1,
                    StubPlacement::AfterProxyMerging,
                    "proxyAlways recordings belong after the proxy stub and merge on equal predicates"
                );
            }

            manager.delete_all().await;
        }

        // AC6: a store that refuses to commit "Recorded" because its publication failed leaves the
        // signature retryable — the claim is released, and the client still gets the upstream
        // response because the upstream call itself succeeded.
        #[tokio::test]
        async fn complete_failure_releases_claim_end_to_end() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, true));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let to = format!("http://127.0.0.1:{up}");
            let proxy_port = generating_proxy_imposter(&manager, &to, "proxyOnce").await;

            for _ in 0..2 {
                let body = reqwest::get(format!("http://127.0.0.1:{proxy_port}/unpublished"))
                    .await
                    .expect("request")
                    .text()
                    .await
                    .expect("body");
                assert_eq!(body, "UP", "the client still gets the upstream response");
            }

            assert_eq!(
                spy.completions.lock().len(),
                2,
                "the second request could claim again, so the signature never wedged"
            );
            assert_eq!(
                spy.releases.lock().len(),
                2,
                "a failed complete releases the claim, exactly as a failed record does"
            );
            assert!(
                spy.lookup(
                    proxy_port,
                    &RequestSignature::new("GET", "/unpublished", None, &[])
                )
                .is_none(),
                "nothing is recorded when publication failed"
            );

            manager.delete_all().await;
        }

        // AC1/AC4: `addWaitBehavior` is the other way a stub gets generated, and it reaches the
        // same seam — the store is handed the stub, latency included, not just for generators.
        #[tokio::test]
        async fn add_wait_behavior_alone_reaches_the_publication_seam() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{ "responses": [{ "proxy": {
                        "to": format!("http://127.0.0.1:{up}"),
                        "mode": "proxyOnce",
                        "addWaitBehavior": true
                    } }] }]
                })))
                .await
                .expect("create wait-behavior proxy imposter");

            let _ = reqwest::get(format!("http://127.0.0.1:{proxy_port}/waited")).await;

            {
                let completions = spy.completions.lock();
                assert_eq!(
                    completions.len(),
                    1,
                    "addWaitBehavior generates a stub, so the claim settles via complete()"
                );
                let stub = serde_json::to_value(&completions[0].3).expect("stub serializes");
                let behaviors = stub
                    .pointer("/responses/0/behaviors")
                    .and_then(serde_json::Value::as_array)
                    .expect("the recorded stub carries behaviors");
                assert!(
                    behaviors.iter().any(|b| b.get("wait").is_some()),
                    "the store receives the stub carrying the captured latency, not a bare shell"
                );
            }
            assert!(spy.records.lock().is_empty());

            manager.delete_all().await;
        }

        // AC1/AC6 + issue #498: when predicate generation FAILS there is no stub, so nothing may be
        // published, and the client still gets the generator-error marker. Since #1333 no claim is
        // taken either: without generated predicates the request has no identity to record or
        // replay under.
        #[cfg(feature = "javascript")]
        #[tokio::test]
        async fn failed_generation_takes_no_claim_and_publishes_nothing() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{ "responses": [{ "proxy": {
                        "to": format!("http://127.0.0.1:{up}"),
                        "mode": "proxyOnce",
                        "predicateGenerators": [
                            { "inject": "function (config, logger, preds) { throw new Error('boom'); }" }
                        ]
                    } }] }]
                })))
                .await
                .expect("create failing-generator proxy imposter");

            let response = reqwest::get(format!("http://127.0.0.1:{proxy_port}/boom"))
                .await
                .expect("request");
            assert!(
                response.headers().contains_key("x-rift-generator-error"),
                "the generation failure stays client-visible (issue #498)"
            );

            assert!(
                spy.completions.lock().is_empty(),
                "no stub was built, so nothing may be offered for publication"
            );
            assert!(
                spy.records.lock().is_empty(),
                "no claim was taken, so nothing is recorded under a key without the identity"
            );
            assert_eq!(
                manager.get_imposter(proxy_port).unwrap().stub_count(),
                1,
                "and no match-all stub is inserted locally either"
            );

            manager.delete_all().await;
        }

        // AC1: with no generators or behaviors configured no stub is built, so there is nothing to
        // publish and the claim settles through the pre-#910 `record` path.
        #[tokio::test]
        async fn no_generated_stub_still_settles_through_record() {
            raise_fd_limit();
            let spy = Arc::new(PublishingProxyStore::new(true, false));
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            let up = upstream(&manager).await;
            let proxy_port = proxy_imposter(&manager, &format!("http://127.0.0.1:{up}")).await;

            let _ = reqwest::get(format!("http://127.0.0.1:{proxy_port}/plain")).await;

            assert_eq!(
                spy.records.lock().len(),
                1,
                "no stub to publish, so the recording settles via record()"
            );
            assert!(spy.completions.lock().is_empty());

            manager.delete_all().await;
        }
    }

    // ===== Per-core listener fan-out (RFC-712, issue #745) =====
    mod per_core_fanout {
        use super::*;

        /// K OS threads each driving a parked current-thread runtime — the shape
        /// `WorkerSet` (rift-http-proxy) provides in production. Handles are only useful
        /// while a thread drives the runtime, so a bare `Runtime` object would not do.
        /// `pub(super)` so the issue-#143 tests can reuse it: the fan-out topology is the one
        /// production actually runs, so the unbound path has to be proven under it too.
        pub(super) struct TestRuntimes {
            pub(super) handles: Vec<tokio::runtime::Handle>,
            stop: Vec<tokio::sync::oneshot::Sender<()>>,
            joins: Vec<std::thread::JoinHandle<()>>,
        }

        pub(super) fn test_runtimes(n: usize) -> TestRuntimes {
            // Each current-thread runtime costs an epoll/kqueue fd + wakeup pipe; at
            // --test-threads=16 a low fd limit makes the builds fail with EMFILE (#814).
            raise_fd_limit();
            let mut handles = Vec::new();
            let mut stop = Vec::new();
            let mut joins = Vec::new();
            for _ in 0..n {
                let (handle_tx, handle_rx) = std::sync::mpsc::channel();
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
                joins.push(std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("test runtime");
                    handle_tx.send(rt.handle().clone()).expect("export handle");
                    let _ = rt.block_on(stop_rx);
                }));
                handles.push(handle_rx.recv().expect("test runtime handle"));
                stop.push(stop_tx);
            }
            TestRuntimes {
                handles,
                stop,
                joins,
            }
        }

        impl TestRuntimes {
            pub(super) fn shutdown(self) {
                for s in self.stop {
                    let _ = s.send(());
                }
                for j in self.joins {
                    let _ = j.join();
                }
            }
        }

        // AC (#745): under per-core fan-out, a create spawns one accept loop per runtime and
        // the port serves requests.
        #[tokio::test]
        async fn fanout_spawns_one_accept_loop_per_runtime_and_serves() {
            let rts = test_runtimes(3);
            let manager = ImposterManager::new().with_accept_runtimes(rts.handles.clone());
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("fanout-ok")]
                })))
                .await
                .expect("create under per-core fan-out");

            let imposter = manager.imposters.get(port_0).expect("imposter registered");
            assert_eq!(
                imposter.serve_handles.lock().len(),
                3,
                "one accept loop per worker runtime"
            );

            for _ in 0..8 {
                let body = reqwest::get(format!("http://127.0.0.1:{port_0}/fanout-ok"))
                    .await
                    .expect("request")
                    .text()
                    .await
                    .expect("body");
                assert!(body.contains("fanout-ok"), "served by the fan-out: {body}");
            }

            manager.delete_imposter(port_0).await.expect("delete");
            rts.shutdown();
        }

        // AC (#745): delete closes EVERY listener in the group and drains. A plain
        // (non-SO_REUSEPORT) bind succeeds only when no group member still holds the port —
        // the strongest cross-platform proof that all N listeners are gone.
        #[tokio::test]
        async fn delete_closes_every_listener_and_frees_the_port() {
            let rts = test_runtimes(3);
            let manager = ImposterManager::new().with_accept_runtimes(rts.handles.clone());
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let _ = reqwest::get(format!("http://127.0.0.1:{port_0}/x"))
                .await
                .expect("request");

            manager.delete_imposter(port_0).await.expect("delete");

            let plain = tokio::net::TcpListener::bind(("127.0.0.1", port_0)).await;
            assert!(
                plain.is_ok(),
                "a plain bind must succeed after delete — a leftover fan-out listener would hold the port: {:?}",
                plain.err()
            );
            rts.shutdown();
        }

        // AC (#745): create is all-or-nothing — a bind failure leaves no claim and no stray
        // listeners, and the port is creatable once the conflict clears.
        #[tokio::test]
        async fn create_is_all_or_nothing_on_bind_failure() {
            let rts = test_runtimes(2);
            let manager = ImposterManager::new().with_accept_runtimes(rts.handles.clone());
            // A plain listener occupies the port: the reusable group cannot join it.
            let blocker = dual_stack_holder();
            let port_0 = blocker.local_addr().expect("blocker addr").port();

            let err = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": port_0, "stubs": [stub_json("ok")]
                })))
                .await
                .expect_err("create must fail while the port is plainly occupied");
            assert!(
                matches!(err, ImposterError::BindError(pat_port, _) if pat_port == port_0),
                "bind failure surfaces as BindError, got {err:?}"
            );
            assert!(
                !manager.imposters.contains(port_0),
                "a failed create must not leave a port claim behind"
            );

            drop(blocker);
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": port_0, "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create succeeds once the conflict clears");
            manager.delete_imposter(port_0).await.expect("delete");
            rts.shutdown();
        }
    }

    // =========================================================================
    // Issue #143 (rift-cluster RFC-001 §7.4.6): serve an imposter whose port could
    // not be bound. Opt-in, apply-path only — a clustered node must keep serving
    // the imposter in-process (front door / gateway both resolve through the port
    // map) when an unrelated process holds its port on that one node.
    // =========================================================================
    mod serve_unbound {
        use super::*;

        /// The squat a real deployment sees: an unrelated process holding the port with a
        /// plain (non-SO_REUSEPORT) listener, so the reusable bind cannot join it. The squatter
        /// takes an OS-assigned port and keeps it, so nothing else can claim it first.
        async fn squat() -> (std::net::TcpListener, u16) {
            let listener = dual_stack_holder();
            let port = listener.local_addr().expect("squatter addr").port();
            (listener, port)
        }

        fn cfg(port: u16, body: &str) -> ImposterConfig {
            imposter_cfg(json!({
                "protocol": "http", "port": port, "stubs": [stub_json(body)]
            }))
        }

        fn failure_for(report: &ApplyReport, port: u16) -> Option<&ImposterError> {
            report
                .failed
                .iter()
                .find(|(p, _)| *p == port)
                .map(|(_, e)| e)
        }

        // Issue #1253: `apply_one` runs the rebind arm too.
        #[tokio::test]
        async fn apply_one_rebinds_an_unbound_imposter() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            let report = manager
                .apply_one(cfg(port_0, "healed"))
                .await
                .expect("apply_one");
            assert!(matches!(
                failure_for(&report, port_0),
                Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
            ));
            assert!(
                !manager
                    .imposters
                    .get(port_0)
                    .expect("registered")
                    .is_bound()
            );

            let report = manager
                .apply_one(cfg(port_0, "healed"))
                .await
                .expect("apply_one");
            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "still squatted: {report:?}"
            );
            assert!(!report.created.contains(&port_0));

            drop(blocker);
            let report = manager
                .apply_one(cfg(port_0, "healed"))
                .await
                .expect("apply_one");
            assert!(report.created.contains(&port_0), "{report:?}");
            assert!(failure_for(&report, port_0).is_none());
            assert!(
                manager
                    .imposters
                    .get(port_0)
                    .expect("registered")
                    .is_bound()
            );
            manager.delete_imposter(port_0).await.expect("delete");
        }

        // AC6: the flag is opt-in. With it off (the OSS default) a bind failure on the
        // apply path stays exactly as it was — no map entry at all.
        #[tokio::test]
        async fn serve_unbound_defaults_off() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new();

            let report = manager
                .apply_config(vec![cfg(port_0, "ok")])
                .await
                .expect("apply validates");

            assert!(
                !manager.imposters.contains(port_0),
                "default-off must not register an imposter whose port could not be bound"
            );
            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "the bind failure is still reported: {:?}",
                report.failed
            );
            drop(blocker);
        }

        // AC1 + AC2: with the flag on, the imposter is registered in the port map (which is
        // what both in-process addressing routes resolve through) and the bind failure is
        // still reported under `failed` — NOT `created`, so the cluster gauge and the
        // `local-engine=` warning keep firing off exactly the field they already read.
        #[tokio::test]
        async fn an_unbound_apply_registers_the_imposter_and_reports_the_bind_failure() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);

            let report = manager
                .apply_config(vec![cfg(port_0, "degraded")])
                .await
                .expect("apply validates");

            let imposter = manager
                .imposters
                .get(port_0)
                .expect("the imposter is registered despite the failed bind");
            assert!(
                !imposter.is_bound(),
                "it is registered without a listener, so it reports itself unbound"
            );
            assert_eq!(
                imposter.config.port,
                Some(port_0),
                "registered under the port it was asked for"
            );
            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "the bind failure is reported: {:?}",
                report.failed
            );
            assert!(
                !report.created.contains(&port_0),
                "an unbound imposter is not a completed create — reporting it as one would \
                 clear the cluster apply-failure entry and the gauge with it"
            );
            drop(blocker);
        }

        // AC3: rebind healing. Once the squatter exits, the next apply of the SAME config
        // must re-attempt the bind and start serving for real — without this the map entry
        // makes reconcile see "exists, unchanged" and the node never heals.
        #[tokio::test]
        async fn an_unbound_imposter_rebinds_on_the_next_apply() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            manager
                .apply_config(vec![cfg(port_0, "healed")])
                .await
                .expect("apply validates");
            assert!(
                !manager
                    .imposters
                    .get(port_0)
                    .expect("registered")
                    .is_bound()
            );

            drop(blocker);

            let report = manager
                .apply_config(vec![cfg(port_0, "healed")])
                .await
                .expect("apply validates");

            assert!(
                manager
                    .imposters
                    .get(port_0)
                    .expect("still registered")
                    .is_bound(),
                "the next apply rebinds the listener once the port is free"
            );
            assert!(
                report.created.contains(&port_0),
                "a successful rebind reports the port as created — that is what clears the \
                 cluster apply-failure entry and the gauge: {report:?}"
            );
            assert!(
                failure_for(&report, port_0).is_none(),
                "nothing failed on the healing apply: {:?}",
                report.failed
            );

            let body = reqwest::get(format!("http://127.0.0.1:{port_0}/healed"))
                .await
                .expect("request")
                .text()
                .await
                .expect("body");
            assert!(
                body.contains("healed"),
                "the rebound listener serves: {body}"
            );

            manager.delete_imposter(port_0).await.expect("delete");
        }

        // The rebind attempt must keep re-reporting while it keeps failing, so the cluster
        // failure entry carries a current reason rather than a stale one from the first apply.
        #[tokio::test]
        async fn a_still_squatted_port_reports_the_failure_on_every_apply() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            manager
                .apply_config(vec![cfg(port_0, "still-down")])
                .await
                .expect("apply validates");

            let report = manager
                .apply_config(vec![cfg(port_0, "still-down")])
                .await
                .expect("apply validates");

            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "a failed rebind is reported again: {:?}",
                report.failed
            );
            assert!(
                !report.created.contains(&port_0),
                "a failed rebind is not a create"
            );
            assert!(
                manager.imposters.contains(port_0),
                "and the entry stays, so the node keeps serving in-process"
            );
            drop(blocker);
        }

        // AC4: the flag is scoped to the apply path. A direct `create_imposter` — the OSS
        // admin/FFI contract — stays all-or-nothing even with the flag on, because `Err`
        // there means "the imposter does not exist" to every existing caller.
        #[tokio::test]
        async fn serve_unbound_does_not_change_direct_create() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);

            let err = manager
                .create_imposter(cfg(port_0, "ok"))
                .await
                .expect_err("a direct create still fails on a bind failure");

            assert!(
                matches!(err, ImposterError::BindError(pat_port, _) if pat_port == port_0),
                "surfaced as BindError, got {err:?}"
            );
            assert!(
                !manager.imposters.contains(port_0),
                "a failed direct create leaves no claim behind, flag or no flag"
            );
            drop(blocker);
        }

        // The degradation is scoped to `BindError`. `PortInUse` is a map-level conflict — an entry
        // for that port already exists — so registering "without a listener" would mean silently
        // overwriting or shadowing a live imposter. It must stay a hard failure even with the flag
        // on, and nothing else pins that: broadening the match guard to all errors would corrupt
        // the port map and every other test here would still pass.
        #[tokio::test]
        async fn port_in_use_still_fails_hard_with_the_flag_on() {
            let manager = ImposterManager::new().with_serve_unbound(true);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("first")]
                })))
                .await
                .expect("create");
            let first = manager.imposters.get(port_0).expect("registered");
            assert!(first.is_bound(), "the first create bound the port for real");

            // Straight at the create path, not through `apply_config` (which would reconcile the
            // existing entry rather than attempt a second claim).
            let err = manager
                .create_imposter(cfg(port_0, "second"))
                .await
                .expect_err("a second create for a claimed port must fail");
            assert!(
                matches!(err, ImposterError::PortInUse(pat_port) if pat_port == port_0),
                "a map-level conflict stays PortInUse, not a degraded registration: {err:?}"
            );
            assert_eq!(
                next_body(
                    &manager
                        .imposters
                        .get(port_0)
                        .expect("still registered")
                        .snapshot()
                        .stubs()[0]
                ),
                "first",
                "the live imposter was not replaced by the refused create"
            );

            manager.delete_imposter(port_0).await.expect("delete");
        }

        // The other half of the scope guard: an auto-assigned port is minted BY the bind, so a
        // failed bind leaves no port to register under. It must fail hard rather than degrade —
        // `explicit_port: None` must never reach the degradation arm.
        #[tokio::test]
        async fn an_auto_assigned_port_still_fails_hard_with_the_flag_on() {
            let manager = ImposterManager::new().with_serve_unbound(true);
            let report = manager
                .apply_config(vec![imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("auto")]
                }))])
                .await
                .expect("apply validates");

            // Auto-assign normally succeeds, so this asserts the shape rather than forcing a
            // failure: whatever happened, nothing unbound may be registered. The port-less config
            // reports failures under the `0` sentinel, which no imposter can be registered as.
            for port in manager.imposters.ports() {
                assert!(
                    manager.imposters.get(port).expect("listed").is_bound(),
                    "an auto-assigned imposter is never registered unbound (port {port})"
                );
            }
            assert!(
                !manager.imposters.contains(0),
                "the port-0 failure sentinel is never a registered imposter: {report:?}"
            );

            manager.delete_all().await;
        }

        // AC5: delete of an unbound imposter is a clean no-op teardown — it has no accept
        // loops to await and no connections to drain.
        #[tokio::test]
        async fn deleting_an_unbound_imposter_is_clean() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            manager
                .apply_config(vec![cfg(port_0, "ok")])
                .await
                .expect("apply validates");
            assert!(manager.imposters.contains(port_0));

            manager
                .delete_imposter(port_0)
                .await
                .expect("deleting an unbound imposter succeeds");

            assert!(
                !manager.imposters.contains(port_0),
                "the entry is gone after delete"
            );
            drop(blocker);
        }

        // A stub-only edit reconciles in place on an unbound imposter: it keeps serving the
        // new stubs in-process and stays unbound, and the bind failure is still reported so
        // the patch does not silently clear the node's degraded state.
        #[tokio::test]
        async fn a_stub_patch_applies_in_place_and_the_imposter_stays_unbound() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            manager
                .apply_config(vec![cfg(port_0, "first")])
                .await
                .expect("apply validates");

            let report = manager
                .apply_config(vec![cfg(port_0, "second")])
                .await
                .expect("apply validates");

            let imposter = manager.imposters.get(port_0).expect("still registered");
            assert!(
                !imposter.is_bound(),
                "still unbound — the port is still squatted"
            );
            assert_eq!(
                next_body(&imposter.snapshot().stubs()[0]),
                "second",
                "the stub edit reached the in-process imposter"
            );
            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "the node is still degraded and still says so: {:?}",
                report.failed
            );
            drop(blocker);
        }

        // An imposter-level edit takes the wholesale-replace path, which tears the old
        // generation down first. That path must honour the flag too: without it, editing an
        // imposter on a squatted port deletes the entry and the node silently STOPS serving —
        // the exact regression this feature exists to prevent.
        #[tokio::test]
        async fn replacing_an_unbound_imposter_keeps_it_registered() {
            let (blocker, port_0) = squat().await;
            let manager = ImposterManager::new().with_serve_unbound(true);
            manager
                .apply_config(vec![cfg(port_0, "before")])
                .await
                .expect("apply validates");

            // `allowCors` is an imposter-level field, so this is a replace, not a stub patch.
            let report = manager
                .apply_config(vec![imposter_cfg(json!({
                    "protocol": "http", "port": port_0, "allowCors": true,
                    "stubs": [stub_json("after")]
                }))])
                .await
                .expect("apply validates");

            assert!(
                manager.imposters.contains(port_0),
                "a replace whose rebind fails must still leave the node serving"
            );
            let imposter = manager.imposters.get(port_0).expect("registered");
            assert!(!imposter.is_bound());
            assert!(
                imposter.config.allow_cors.enabled(),
                "the replacement config is the one now serving"
            );
            assert!(
                matches!(
                    failure_for(&report, port_0),
                    Some(ImposterError::BindError(pat_port, _)) if *pat_port == port_0
                ),
                "still reported as degraded: {:?}",
                report.failed
            );
            drop(blocker);
        }

        // The production topology is per-core fan-out (RFC-712), where the listener GROUP is
        // bound in a second step after the probe bind. The degradation must cover that step
        // too, or clustered deployments — the only ones that turn this flag on — still fail
        // hard.
        #[tokio::test]
        async fn unbound_registration_works_under_per_core_fanout() {
            let (blocker, port_0) = squat().await;
            let rts = super::per_core_fanout::test_runtimes(2);
            let manager = ImposterManager::new()
                .with_serve_unbound(true)
                .with_accept_runtimes(rts.handles.clone());

            manager
                .apply_config(vec![cfg(port_0, "fanout-degraded")])
                .await
                .expect("apply validates");

            let imposter = manager
                .imposters
                .get(port_0)
                .expect("registered under fan-out too");
            assert!(!imposter.is_bound());
            assert_eq!(
                imposter.serve_handles.lock().len(),
                0,
                "no accept loops were spawned for a port that never bound"
            );

            drop(blocker);
            manager
                .apply_config(vec![cfg(port_0, "fanout-degraded")])
                .await
                .expect("apply validates");
            assert_eq!(
                manager
                    .imposters
                    .get(port_0)
                    .expect("registered")
                    .serve_handles
                    .lock()
                    .len(),
                2,
                "the rebind restores the full per-runtime listener group"
            );

            manager.delete_imposter(port_0).await.expect("delete");
            rts.shutdown();
        }
    }

    // =========================================================================
    // Issue #819: NoMatchInterceptor — last-chance hook on the no-match path
    // =========================================================================
    mod no_match_interceptor {
        use super::*;
        use crate::extensions::no_match::{NoMatchContext, NoMatchDirective, NoMatchInterceptor};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counts calls and returns a fixed directive; optionally installs a matching stub the
        /// first time it is consulted, standing in for "the replicated config caught up".
        struct Spy {
            calls: AtomicUsize,
            directive: NoMatchDirective,
            /// The stub the rescue installs when consulted.
            rescue_stub: serde_json::Value,
            /// Set AFTER the manager is built (the manager needs the spy at construction), so
            /// the rescue adds its stub to the very manager serving the request.
            rescue_with: Mutex<Option<(Arc<ImposterManager>, u16)>>,
            seen: Mutex<Vec<(u16, String, String)>>,
        }

        impl Spy {
            fn new(directive: NoMatchDirective) -> Arc<Self> {
                Arc::new(Self {
                    calls: AtomicUsize::new(0),
                    directive,
                    rescue_with: Mutex::new(None),
                    rescue_stub: serde_json::Value::Null,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn rescuing() -> Arc<Self> {
                Self::rescuing_with_stub(json!({
                    "predicates": [{"equals": {"path": "/late"}}],
                    "responses": [{"is": {"statusCode": 200, "body": "rescued"}}]
                }))
            }

            fn rescuing_with_stub(stub: serde_json::Value) -> Arc<Self> {
                Arc::new(Self {
                    calls: AtomicUsize::new(0),
                    directive: NoMatchDirective::RetryMatch,
                    rescue_with: Mutex::new(None),
                    rescue_stub: stub,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn arm_rescue(&self, manager: Arc<ImposterManager>, port: u16) {
                *self.rescue_with.lock() = Some((manager, port));
            }

            fn calls(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }
        }

        impl NoMatchInterceptor for Spy {
            fn on_no_match<'a>(
                &'a self,
                ctx: NoMatchContext<'a>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = NoMatchDirective> + Send + 'a>>
            {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.seen
                    .lock()
                    .push((ctx.port, ctx.method.to_string(), ctx.path.to_string()));
                let rescue = self.rescue_with.lock().clone();
                let stub_json = self.rescue_stub.clone();
                let directive = self.directive;
                Box::pin(async move {
                    if let Some((manager, port)) = rescue {
                        let stub: Stub = serde_json::from_value(stub_json).expect("late stub");
                        manager
                            .add_stub(port, stub, None)
                            .await
                            .expect("add late stub");
                    }
                    directive
                })
            }
        }

        async fn get(port: u16, path: &str) -> (u16, String, bool) {
            let resp = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
                .await
                .expect("request");
            let status = resp.status().as_u16();
            let no_match = resp.headers().contains_key("x-rift-no-match");
            (status, resp.text().await.unwrap_or_default(), no_match)
        }

        // AC1: RetryMatch rescues a request once the stub appears — served as a normal match,
        // no x-rift-no-match, interceptor consulted exactly once.
        #[tokio::test]
        async fn retry_match_rescues_after_stub_appears() {
            let spy = Spy::rescuing();
            let manager = Arc::new(
                ImposterManager::new()
                    .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>),
            );
            // Armed after construction so the rescue mutates the manager actually serving.
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": []
                })))
                .await
                .expect("create");
            spy.arm_rescue(Arc::clone(&manager), port_0);

            let (status, body, no_match) = get(port_0, "/late").await;
            assert_eq!(status, 200);
            assert_eq!(
                body, "rescued",
                "the rescued retry must serve the late stub"
            );
            assert!(
                !no_match,
                "a rescued request is a normal match, not a no-match"
            );
            assert_eq!(spy.calls(), 1, "consulted exactly once");
            assert_eq!(
                spy.seen.lock().first().cloned(),
                Some((port_0, "GET".to_string(), "/late".to_string())),
                "the context must carry the bound port, method and path"
            );

            manager.delete_all().await;
        }

        // AC2: a second miss falls through exactly as Proceed would — and only one retry happens.
        #[tokio::test]
        async fn second_miss_falls_through_and_retries_only_once() {
            let spy = Spy::new(NoMatchDirective::RetryMatch);
            let manager = ImposterManager::new()
                .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": []
                })))
                .await
                .expect("create");

            let (status, _body, no_match) = get(port_0, "/nope").await;
            assert_eq!(status, 200);
            assert!(
                no_match,
                "a second miss falls through to the empty-200 no-match"
            );
            assert_eq!(spy.calls(), 1, "at most one retry per request — no loop");

            manager.delete_all().await;
        }

        // AC3: a MATCHED request never consults the interceptor (hot path pays nothing).
        #[tokio::test]
        async fn matched_requests_never_consult_the_interceptor() {
            let spy = Spy::new(NoMatchDirective::RetryMatch);
            let manager = ImposterManager::new()
                .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/hit"}}],
                        "responses": [{"is": {"statusCode": 200, "body": "hit"}}]
                    }]
                })))
                .await
                .expect("create");

            let (status, body, _) = get(port_0, "/hit").await;
            assert_eq!(status, 200);
            assert_eq!(body, "hit");
            assert_eq!(spy.calls(), 0, "a matched request must not reach the hook");

            manager.delete_all().await;
        }

        // AC4: Proceed is inert — the response is the untouched baseline fallthrough.
        #[tokio::test]
        async fn proceed_serves_the_baseline_fallthrough() {
            let spy = Spy::new(NoMatchDirective::Proceed);
            let manager = ImposterManager::new()
                .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [],
                    "defaultResponse": {"statusCode": 503, "body": "default"}
                })))
                .await
                .expect("create");

            let (status, body, _) = get(port_0, "/nope").await;
            assert_eq!(status, 503, "Proceed must fall through to defaultResponse");
            assert_eq!(body, "default");
            assert_eq!(spy.calls(), 1, "Proceed is still consulted once");

            manager.delete_all().await;
        }

        // AC5: a disabled imposter never consults the interceptor — the enabled gate precedes
        // matching, so the hook is unreachable there.
        #[tokio::test]
        async fn disabled_imposter_never_consults_the_interceptor() {
            let spy = Spy::new(NoMatchDirective::RetryMatch);
            let manager = ImposterManager::new()
                .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>);
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": []
                })))
                .await
                .expect("create");
            manager
                .set_imposter_enabled(port_0, false)
                .await
                .expect("disable the imposter");

            let _ = get(port_0, "/nope").await;
            assert_eq!(
                spy.calls(),
                0,
                "the enabled gate precedes matching, so a disabled imposter never reaches the hook"
            );

            manager.delete_all().await;
        }

        // AC6 (the headline design ruling, #819): the hook fires even when a default IS
        // configured, and a rescue OUTRANKS it. Without this test, an "optimization" that only
        // consults the interceptor when no default exists would keep every other test green while
        // silently misdirecting lagging requests upstream.
        #[tokio::test]
        async fn rescue_outranks_a_configured_default_response() {
            let spy = Spy::rescuing();
            let manager = Arc::new(
                ImposterManager::new()
                    .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>),
            );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http",
                    "stubs": [],
                    "defaultResponse": {"statusCode": 503, "body": "default"}
                })))
                .await
                .expect("create");
            spy.arm_rescue(Arc::clone(&manager), port_0);

            let (status, body, _) = get(port_0, "/late").await;
            assert_eq!(
                status, 200,
                "a rescued match must win over the configured defaultResponse"
            );
            assert_eq!(body, "rescued");
            assert_eq!(spy.calls(), 1);

            manager.delete_all().await;
        }

        // AC7 (#819): the retry must pass the IDENTICAL request facts as the first attempt. The
        // rescue stub predicates on body, query and a header, so dropping any of them from the
        // retry call (passing None/empty) fails here while a bodyless-GET-only suite would not
        // notice.
        #[tokio::test]
        async fn retry_passes_the_identical_request_facts() {
            let spy = Spy::rescuing_with_stub(json!({
                "predicates": [{"and": [
                    {"equals": {"path": "/echo", "method": "POST"}},
                    {"equals": {"query": {"tenant": "acme"}}},
                    {"equals": {"headers": {"x-trace": "abc123"}}},
                    {"contains": {"body": "payload"}}
                ]}],
                "responses": [{"is": {"statusCode": 200, "body": "facts-preserved"}}]
            }));
            let manager = Arc::new(
                ImposterManager::new()
                    .with_no_match_interceptor(Arc::clone(&spy) as Arc<dyn NoMatchInterceptor>),
            );
            let port_0 = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": []
                })))
                .await
                .expect("create");
            spy.arm_rescue(Arc::clone(&manager), port_0);

            let resp = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port_0}/echo?tenant=acme"))
                .header("x-trace", "abc123")
                .body("a payload here")
                .send()
                .await
                .expect("request");
            assert_eq!(resp.status().as_u16(), 200);
            assert_eq!(
                resp.text().await.unwrap_or_default(),
                "facts-preserved",
                "the retry must see the same method/path/query/headers/body as the first attempt"
            );

            manager.delete_all().await;
        }
    }

    // =========================================================================
    // Issue #1517: host-less imposters inherit the server's `--host`
    // =========================================================================
    mod default_host {
        use super::*;
        use std::net::{IpAddr, SocketAddr};
        use std::time::Duration;

        fn has_ipv6_loopback() -> bool {
            std::net::TcpListener::bind("[::1]:0").is_ok()
        }

        fn accepts(ip: IpAddr, port: u16) -> bool {
            std::net::TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2))
                .is_ok()
        }

        const V4_LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        const V6_LOOPBACK: IpAddr = IpAddr::V6(std::net::Ipv6Addr::LOCALHOST);

        /// A loopback-confined imposter answers `127.0.0.1` and not `[::1]`, which the dual-stack
        /// `[::]` a host-less imposter otherwise binds would answer. Without IPv6 the two cannot
        /// be told apart this way, so the `[::1]` half is skipped.
        fn assert_v4_loopback_only(port: u16) {
            assert!(accepts(V4_LOOPBACK, port), "127.0.0.1:{port} must answer");
            if has_ipv6_loopback() {
                assert!(
                    !accepts(V6_LOOPBACK, port),
                    "[::1]:{port} must refuse: the imposter inherited 127.0.0.1"
                );
            }
        }

        fn host_less(port: Option<u16>) -> ImposterConfig {
            let mut v = json!({"protocol": "http", "stubs": [stub_json("inherited")]});
            if let Some(p) = port {
                v["port"] = json!(p);
            }
            imposter_cfg(v)
        }

        #[tokio::test]
        async fn a_host_less_imposter_binds_the_default_host() {
            let manager = ImposterManager::new().with_default_host(Some("127.0.0.1".to_string()));
            for explicit in [Some(free_port()), None] {
                let port = manager
                    .create_imposter(host_less(explicit))
                    .await
                    .expect("create");
                assert_v4_loopback_only(port);
                assert_eq!(
                    manager.imposters.get(port).expect("registered").config.host,
                    None,
                    "the inherited host is never written into the config"
                );
            }
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn an_imposter_host_overrides_the_default_host() {
            if !has_ipv6_loopback() {
                return;
            }
            let manager = ImposterManager::new().with_default_host(Some("127.0.0.1".to_string()));
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "host": "::", "stubs": [stub_json("own")]
                })))
                .await
                .expect("create");
            assert!(
                accepts(V6_LOOPBACK, port),
                "the imposter's own `::` wins over the default 127.0.0.1"
            );
            manager.delete_all().await;
        }

        #[tokio::test]
        async fn no_default_host_keeps_the_dual_stack_bind() {
            if !has_ipv6_loopback() {
                return;
            }
            let manager = ImposterManager::new().with_default_host(None);
            let port = manager
                .create_imposter(host_less(None))
                .await
                .expect("create");
            assert!(accepts(V4_LOOPBACK, port) && accepts(V6_LOOPBACK, port));
            manager.delete_all().await;
        }

        // The #143 heal binds where the create would have, so an imposter that could not bind at
        // apply time does not come back on every interface once its port frees up.
        #[tokio::test]
        async fn a_rebind_uses_the_default_host() {
            // A plain (non-reusable) listener on the exact address the imposter will bind, so the
            // reusable bind cannot join it.
            let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("squat");
            let port = squatter.local_addr().expect("squat addr").port();
            let manager = ImposterManager::new()
                .with_serve_unbound(true)
                .with_default_host(Some("127.0.0.1".to_string()));
            manager
                .apply_config(vec![host_less(Some(port))])
                .await
                .expect("apply validates");
            assert!(
                !manager.imposters.get(port).expect("registered").is_bound(),
                "the squatted port leaves the imposter unbound"
            );

            drop(squatter);
            manager
                .apply_config(vec![host_less(Some(port))])
                .await
                .expect("apply validates");
            assert!(manager.imposters.get(port).expect("registered").is_bound());
            assert_v4_loopback_only(port);
            manager.delete_all().await;
        }
    }

    // Issue #1523: Mountebank's `--localOnly` refuses every client outside the loopback interface on
    // every imposter (`util/ip.js:5-52`, `models/imposter.js:103-107`). The policy is a manager
    // property, checked at accept, so a refused client never reaches the handler: nothing is
    // recorded or counted, and an HTTPS client gets no handshake.
    mod peer_policy {
        use super::*;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const V4_LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

        /// The address this host routes off-host traffic from (#1517's UDP trick: a `connect` sends
        /// nothing, it only picks the source). `None` where only loopback exists, and the test
        /// then has nothing to refuse, so it says so and passes.
        fn non_loopback_local_ip() -> Option<IpAddr> {
            let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
            socket.connect("192.0.2.1:9").ok()?;
            let ip = socket.local_addr().ok()?.ip();
            (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
        }

        fn remote_or_skip(test: &str) -> Option<IpAddr> {
            let ip = non_loopback_local_ip();
            if ip.is_none() {
                eprintln!("{test}: skipped, this host has no non-loopback address");
            }
            ip
        }

        /// One `GET /` over a raw socket, returning whatever came back. A refused client's
        /// connection is closed with nothing written, which reads as EOF or a reset depending on
        /// timing, so either is "nothing came back" (an empty string).
        async fn raw_get(ip: IpAddr, port: u16) -> String {
            let connect = tokio::net::TcpStream::connect(SocketAddr::new(ip, port));
            let Ok(Ok(mut stream)) = tokio::time::timeout(Duration::from_secs(2), connect).await
            else {
                return String::new();
            };
            let request = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
            if stream.write_all(request).await.is_err() {
                return String::new();
            }
            let mut buf = Vec::new();
            match tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await {
                // EOF or a reset: keep whatever arrived first.
                Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
                Err(_) => panic!("{ip}:{port}: the connection neither answered nor closed"),
            }
        }

        fn served(response: &str) -> bool {
            response.starts_with("HTTP/1.1 200")
        }

        fn recording(host: Option<&str>, protocol: &str) -> ImposterConfig {
            let mut v = json!({
                "protocol": protocol,
                "recordRequests": true,
                "stubs": [stub_json("local")]
            });
            if let Some(h) = host {
                v["host"] = json!(h);
            }
            imposter_cfg(v)
        }

        /// Loopback is served and recorded; the off-host client gets a closed connection and
        /// leaves no trace in `numberOfRequests` or `requests`.
        async fn assert_refuses_remote(manager: &ImposterManager, port: u16, remote: IpAddr) {
            let local = raw_get(V4_LOOPBACK, port).await;
            assert!(served(&local), "127.0.0.1:{port} must be served: {local:?}");
            let refused = raw_get(remote, port).await;
            assert!(
                refused.is_empty(),
                "{remote}:{port} must get a closed connection with no response under \
                 LoopbackOnly, got {refused:?}"
            );
            let imposter = manager.get_imposter(port).expect("imposter");
            assert_eq!(
                imposter.get_request_count(),
                1,
                "only the loopback request counts"
            );
            let recorded = imposter.get_recorded_requests();
            assert_eq!(recorded.len(), 1, "only the loopback request is recorded");
            assert!(
                recorded[0].request_from.contains("127.0.0.1"),
                "the recorded request is the loopback one: {}",
                recorded[0].request_from
            );
        }

        #[tokio::test]
        async fn loopback_only_refuses_a_remote_client_on_an_ipv4_wildcard_imposter() {
            let Some(remote) = remote_or_skip("ipv4 wildcard") else {
                return;
            };
            let manager = ImposterManager::new().with_peer_policy(PeerPolicy::LoopbackOnly);
            assert_eq!(manager.peer_policy(), PeerPolicy::LoopbackOnly);
            let port = manager
                .create_imposter(recording(Some("0.0.0.0"), "http"))
                .await
                .expect("create");
            assert_refuses_remote(&manager, port, remote).await;
            manager.delete_all().await;
        }

        // A dual-stack listener sees both clients as IPv4-mapped (`::ffff:…`): loopback must still
        // be admitted, the remote still refused. The host-less imposter binds `[::]` too (#1514).
        #[tokio::test]
        async fn loopback_only_canonicalises_ipv4_mapped_peers_on_a_dual_stack_imposter() {
            let Some(remote) = remote_or_skip("dual stack") else {
                return;
            };
            if std::net::TcpListener::bind("[::]:0").is_err() {
                eprintln!("dual stack: skipped, this host has no IPv6");
                return;
            }
            let manager = ImposterManager::new().with_peer_policy(PeerPolicy::LoopbackOnly);
            for host in [Some("::"), None] {
                let port = manager
                    .create_imposter(recording(host, "http"))
                    .await
                    .expect("create");
                assert_refuses_remote(&manager, port, remote).await;
            }
            manager.delete_all().await;
        }

        // Refused before TLS: the remote client's handshake fails because none is attempted.
        #[tokio::test]
        async fn loopback_only_refuses_a_remote_https_client_before_the_handshake() {
            let Some(remote) = remote_or_skip("https") else {
                return;
            };
            let manager = ImposterManager::new().with_peer_policy(PeerPolicy::LoopbackOnly);
            let port = manager
                .create_imposter(recording(Some("0.0.0.0"), "https"))
                .await
                .expect("create");
            let client = reqwest::Client::builder()
                .no_proxy()
                .tls_danger_accept_invalid_certs(true)
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client");
            let local = client
                .get(format!("https://127.0.0.1:{port}/"))
                .send()
                .await
                .expect("loopback is served over TLS");
            assert_eq!(local.status().as_u16(), 200);
            let refused = client.get(format!("https://{remote}:{port}/")).send().await;
            assert!(
                refused.is_err(),
                "{remote}:{port} must fail before a response under LoopbackOnly: {refused:?}"
            );
            assert_eq!(
                manager
                    .get_imposter(port)
                    .expect("imposter")
                    .get_request_count(),
                1
            );
            manager.delete_all().await;
        }

        // The #143 heal goes through `spawn_accept_loops` again; the healed listener still refuses.
        #[tokio::test]
        async fn a_rebound_listener_keeps_refusing_remote_clients() {
            let Some(remote) = remote_or_skip("rebind") else {
                return;
            };
            // A plain listener on the IPv4 wildcard the imposter will bind, so its reusable bind
            // cannot join it.
            let squatter = std::net::TcpListener::bind("0.0.0.0:0").expect("squat");
            let port = squatter.local_addr().expect("squat addr").port();
            let manager = ImposterManager::new()
                .with_serve_unbound(true)
                .with_peer_policy(PeerPolicy::LoopbackOnly);
            let mut cfg = recording(Some("0.0.0.0"), "http");
            cfg.port = Some(port);
            manager
                .apply_config(vec![cfg.clone()])
                .await
                .expect("apply validates");
            assert!(
                !manager.imposters.get(port).expect("registered").is_bound(),
                "the squatted port leaves the imposter unbound"
            );

            drop(squatter);
            manager
                .apply_config(vec![cfg])
                .await
                .expect("apply validates");
            assert!(manager.imposters.get(port).expect("registered").is_bound());
            assert_refuses_remote(&manager, port, remote).await;
            manager.delete_all().await;
        }

        // Pins the default: a manager nobody configured serves every client, as before #1523.
        #[tokio::test]
        async fn the_default_manager_serves_remote_clients() {
            let Some(remote) = remote_or_skip("default") else {
                return;
            };
            let manager = ImposterManager::new();
            assert_eq!(manager.peer_policy(), PeerPolicy::Any);
            let port = manager
                .create_imposter(recording(Some("0.0.0.0"), "http"))
                .await
                .expect("create");
            let response = raw_get(remote, port).await;
            assert!(
                served(&response),
                "{remote}:{port} must be served: {response:?}"
            );
            manager.delete_all().await;
        }
    }

    // Issue #1557: a port the server's own listeners hold is refused to every imposter.
    mod reserved_ports {
        use super::*;

        fn cfg(port: u16) -> ImposterConfig {
            imposter_cfg(json!({
                "protocol": "http", "port": port, "stubs": [stub_json("imposter")]
            }))
        }

        #[tokio::test]
        async fn a_reserved_port_is_refused_on_every_explicit_door() {
            let manager = ImposterManager::new();
            let port = free_port();
            manager.reserve_port(port, ServerListener::Admin);
            assert_eq!(manager.reserved_by(port), Some(ServerListener::Admin));

            let created = manager.create_imposter(cfg(port)).await;
            assert!(
                matches!(created, Err(ImposterError::PortInUse(p)) if p == port),
                "create: {created:?}"
            );
            let applied = manager.apply_config(vec![cfg(port)]).await;
            assert!(
                matches!(applied, Err(ImposterError::PortInUse(p)) if p == port),
                "apply_config: {applied:?}"
            );
            let one = manager.apply_one(cfg(port)).await;
            assert!(
                matches!(one, Err(ImposterError::PortInUse(p)) if p == port),
                "apply_one: {one:?}"
            );
            assert!(!manager.imposters.contains(port), "nothing was created");

            manager.release_port(port);
            assert_eq!(manager.reserved_by(port), None);
            let created = manager.create_imposter(cfg(port)).await;
            assert!(matches!(created, Ok(p) if p == port), "{created:?}");
            assert!(manager.imposter_listens_on(port));
            manager.delete_all().await;
        }

        // The batch pre-check refuses the whole set before anything is touched, as a duplicate
        // port does: the running imposter survives a `PUT` that names a reserved port beside it.
        #[tokio::test]
        async fn apply_config_refuses_the_whole_set_before_touching_anything() {
            let manager = ImposterManager::new();
            let running = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("a")]
                })))
                .await
                .expect("create");
            let reserved = free_port();
            manager.reserve_port(reserved, ServerListener::Metrics);

            let applied = manager.apply_config(vec![cfg(reserved)]).await;
            assert!(
                matches!(applied, Err(ImposterError::PortInUse(p)) if p == reserved),
                "{applied:?}"
            );
            assert!(
                manager.imposters.contains(running),
                "the refused set must not delete the running imposter"
            );
            manager.delete_all().await;
        }

        // Every port reserved: auto-assign refuses each kernel pick and runs out of attempts,
        // rather than handing out a port the server holds.
        #[tokio::test]
        async fn auto_assign_never_hands_out_a_reserved_port() {
            let manager = ImposterManager::new();
            for port in 1..=u16::MAX {
                manager.reserve_port(port, ServerListener::FrontDoor);
            }
            let created = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("a")]
                })))
                .await;
            assert!(
                matches!(created, Err(ImposterError::BindError(0, _))),
                "every candidate is reserved: {created:?}"
            );

            for port in 1..=u16::MAX {
                manager.release_port(port);
            }
            let port = manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "stubs": [stub_json("a")]
                })))
                .await
                .expect("create once released");
            assert!(port != 0);
            manager.delete_all().await;
        }

        // Under serve-unbound a reserved port is a failed bind, as `EADDRINUSE` made it on Linux
        // before the set existed: the imposter is registered without a listener and heals once the
        // port is released.
        #[tokio::test]
        async fn serve_unbound_registers_a_reserved_port_without_a_listener() {
            let manager = ImposterManager::new().with_serve_unbound(true);
            let port = free_port();
            manager.reserve_port(port, ServerListener::Admin);

            let report = manager.apply_config(vec![cfg(port)]).await.expect("apply");
            assert!(
                report
                    .failed
                    .iter()
                    .any(|(p, e)| *p == port && matches!(e, ImposterError::BindError(..))),
                "{report:?}"
            );
            let registered = manager.imposters.get(port).expect("registered");
            assert!(!registered.is_bound());
            assert!(!manager.imposter_listens_on(port));

            manager.release_port(port);
            let report = manager.apply_config(vec![cfg(port)]).await.expect("apply");
            assert!(report.created.contains(&port), "{report:?}");
            assert!(manager.imposter_listens_on(port));
            manager.delete_all().await;
        }
    }
}
