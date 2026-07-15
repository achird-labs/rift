//! ImposterManager - lifecycle management for multiple imposters.
//!
//! This module handles creating, deleting, and managing multiple imposters,
//! each running on its own port.

use super::core::Imposter;
use super::fault_io::{FaultCell, FaultIo, TcpFaultKind};
use super::handler::handle_imposter_request_decorated;
use super::reconcile::{ApplyReport, ImposterEvent, ImposterEventListener, StubReconcile};
use super::types::{ImposterConfig, ImposterError, Stub};
use crate::behaviors::ResponseSequencer;
use crate::extensions::decorate::ResponseDecorator;
use crate::extensions::flow_state::FlowStoreProvider;
use crate::imposter::journal::RequestJournal;
use crate::recording::ProxyRecordingStore;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

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
async fn run_http1<I>(
    io: I,
    imposter: Arc<Imposter>,
    addr: std::net::SocketAddr,
    fault_cell: FaultCell,
    mut conn_shutdown_rx: broadcast::Receiver<()>,
    port: u16,
    decorator: Option<Arc<dyn ResponseDecorator>>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    // A TCP fault (#239) aborts the whole socket, which is meaningless under HTTP/2 stream
    // multiplexing (one stream's fault would tear down every concurrent stream, and the raw
    // HTTP/1 fault bytes are nonsense to an h2 client). So an imposter that can fire a TCP fault
    // is served HTTP/1-only; everything else auto-negotiates HTTP/1 and HTTP/2 (issue #295).
    let http1_only = imposter.uses_tcp_faults() || crate::util::http2_disabled();
    let service = service_fn(move |req| {
        let imposter = Arc::clone(&imposter);
        let fault_cell = Arc::clone(&fault_cell);
        let decorator = decorator.clone();
        async move {
            let response =
                handle_imposter_request_decorated(req, imposter, addr, port, decorator).await?;
            if let Some(kind) = response.extensions().get::<TcpFaultKind>().copied() {
                *fault_cell.lock() = Some(kind);
            }
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
        drive_conn!(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
    } else {
        let builder =
            hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
        drive_conn!(builder.serve_connection(io, service));
    }
}

/// Manages the lifecycle of multiple imposters
pub struct ImposterManager {
    /// Active imposters by port
    imposters: RwLock<HashMap<u16, Arc<Imposter>>>,
    /// Global shutdown signal (for future graceful shutdown)
    shutdown_tx: broadcast::Sender<()>,
    /// Optional data directory for persistence write-through
    datadir: Option<Arc<PathBuf>>,
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
    /// Pluggable proxy-recording backend (issue #315); None = per-imposter LocalProxyStore.
    proxy_store: Option<Arc<dyn ProxyRecordingStore>>,
    /// Upper bound on how long `delete` waits for a torn-down imposter's in-flight connections to
    /// drain before returning anyway with a warning (issue #596). Never configurable at runtime;
    /// overridable only in tests via [`Self::with_conn_drain`].
    conn_drain: Duration,
    /// Broadcast of lifecycle + recorded-request events for the admin SSE stream (issue #461).
    /// Created unconditionally (a `broadcast::Sender` with no receivers is ~free); publishing is a
    /// no-op until a client subscribes.
    event_bus: Arc<super::events::AdminEventBus>,
}

/// Default bound for the post-delete connection drain (issue #596). Generous: normal graceful
/// shutdown of idle/short connections completes in milliseconds, so this only caps a genuinely
/// stalled in-flight request (e.g. a long `wait` behavior) rather than hanging the delete.
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
            imposters: RwLock::new(HashMap::new()),
            shutdown_tx,
            datadir: datadir.map(Arc::new),
            tls_defaults: TlsDefaults::default(),
            event_listener: None,
            response_decorator: None,
            flow_store_provider: None,
            sequencer: None,
            request_journal: None,
            proxy_store: None,
            conn_drain: DEFAULT_CONN_DRAIN,
            event_bus: Arc::new(super::events::AdminEventBus::new()),
        }
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
    /// per imposter (issue #312), consulted before the built-in `_rift.flowState` selection.
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
    /// the GC hook). A single in-place stub replace deliberately does NOT reset: the slot
    /// survives, so slot-keyed cursors keep cycling; a `stub_key`-keyed backend whose key
    /// changed with the content keeps the old key's cursor until port teardown reclaims it.
    #[must_use]
    pub fn with_sequencer(mut self, sequencer: Arc<dyn ResponseSequencer>) -> Self {
        self.sequencer = Some(sequencer);
        self
    }

    /// Register a pluggable recorded-request backend (issue #314), shared across imposters
    /// and keyed by port. Without one, each imposter keeps a private in-memory journal with
    /// the historical semantics (10k cap, clear-resets-count). Imposter deletion clears the
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
    /// [`LocalProxyStore`](crate::recording::LocalProxyStore) for its own proxy mode with the
    /// historical semantics (proxyOnce once-gate, caps) plus the release-on-error fix. Imposter
    /// deletion clears the port's saved responses so a later imposter reusing the port starts
    /// clean.
    ///
    /// A shared store carries a single proxy mode; embedders mixing proxy modes across ports
    /// should keep the per-imposter default instead.
    #[must_use]
    pub fn with_proxy_store(mut self, store: Arc<dyn ProxyRecordingStore>) -> Self {
        self.proxy_store = Some(store);
        self
    }

    fn emit(&self, event: ImposterEvent) {
        if let Some(listener) = &self.event_listener {
            listener.on_event(&event);
        }
        // Additionally fan the event out to the admin SSE bus (issue #461) — separate from the
        // single-slot embedder listener above, which callers may already hold.
        use super::events::ImposterAction;
        let (action, port) = match event {
            ImposterEvent::Created(p) => (ImposterAction::Created, Some(p)),
            ImposterEvent::Replaced(p) => (ImposterAction::Replaced, Some(p)),
            ImposterEvent::StubsChanged(p) => (ImposterAction::StubsChanged, Some(p)),
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

    /// Resolve the TLS acceptor for an HTTPS imposter by precedence: inline imposter cert/key →
    /// server default → self-signed fallback → error (never silent cleartext, issue #206).
    fn resolve_tls_acceptor(
        &self,
        config: &ImposterConfig,
    ) -> Result<tokio_rustls::TlsAcceptor, ImposterError> {
        let from_pem = |cert: &str, key: &str| {
            crate::proxy::tls::tls_acceptor_from_pem(cert.as_bytes(), key.as_bytes())
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
            return crate::proxy::tls::generate_self_signed_acceptor()
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
        let port = self.create_imposter_inner(config).await?;
        self.emit(ImposterEvent::Created(port));
        Ok(port)
    }

    /// Create without emitting an event, so composite operations (wholesale replace in
    /// `apply_config`) can report a single higher-level event instead of Deleted+Created.
    async fn create_imposter_inner(
        &self,
        mut config: ImposterConfig,
    ) -> Result<u16, ImposterError> {
        // Validate protocol first
        match config.protocol.as_str() {
            "http" | "https" => {}
            proto => return Err(ImposterError::InvalidProtocol(proto.to_string())),
        }

        // For HTTPS, resolve the per-imposter TLS acceptor up front so a missing/invalid cert
        // fails loudly at creation rather than silently serving cleartext (issue #206).
        let tls_acceptor = if config.protocol.eq_ignore_ascii_case("https") {
            Some(self.resolve_tls_acceptor(&config)?)
        } else {
            None
        };

        let bind_host: &str = config.host.as_deref().unwrap_or("0.0.0.0");
        // Determine port - either from config or auto-assign
        let (port, listener) = if let Some(p) = config.port {
            // Check if specified port is already in use
            if self.imposters.read().contains_key(&p) {
                return Err(ImposterError::PortInUse(p));
            }
            // Bind with SO_REUSEADDR/REUSEPORT so a hot-reload (#197) can re-bind the same port
            // immediately after the previous imposter's listener is torn down.
            let addr = (bind_host, p)
                .to_socket_addrs()
                .map_err(|e| ImposterError::BindError(p, e.to_string()))?
                .next()
                .ok_or_else(|| ImposterError::BindError(p, "no socket address".to_string()))?;
            (
                p,
                crate::proxy::network::create_reusable_listener(addr)
                    .map_err(|e| ImposterError::BindError(p, e.to_string()))?,
            )
        } else {
            // Auto-assign port: find an available port starting from a base
            self.find_available_port(bind_host).await?
        };

        config.port = Some(port);

        info!("Imposter bound to {}:{}", bind_host, port);
        // Create imposter
        let mut imposter = Imposter::new_with_hooks_and_journal(
            config,
            self.flow_store_provider.as_ref(),
            self.sequencer.clone(),
            self.request_journal.clone(),
        )
        .map_err(|e| ImposterError::FlowStoreConfig(format!("{e:#}")))?;

        // Inject the shared proxy-recording store, if one is registered (issue #315);
        // otherwise the imposter keeps its private per-mode LocalProxyStore.
        if let Some(store) = &self.proxy_store {
            imposter.proxy_store = Arc::clone(store);
        }

        // Share the admin event bus so recorded requests fan out to the SSE stream (issue #461).
        imposter.event_bus = Some(Arc::clone(&self.event_bus));

        // Create shutdown channel for this imposter
        let (shutdown_tx, _) = broadcast::channel(1);
        imposter.shutdown_tx = Some(shutdown_tx.clone());

        let imposter = Arc::new(imposter);

        // Start serving
        let imposter_clone = Arc::clone(&imposter);
        let conn_shutdown_tx = shutdown_tx.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let response_decorator = self.response_decorator.clone();
        // Read socket tuning once per listener, not per accepted connection.
        let socket_tuning = crate::proxy::network::SocketTuning::from_env();

        let serve_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                crate::proxy::network::apply_stream_tuning(&stream, &socket_tuning);
                                let imposter = Arc::clone(&imposter_clone);
                                // Each connection watches the shutdown signal so existing
                                // keep-alive connections are gracefully closed on delete,
                                // not just new connections (issue #207).
                                let conn_shutdown_rx = conn_shutdown_tx.subscribe();
                                // Per-imposter TLS acceptor is cheap to clone (Arc-backed).
                                let tls_acceptor = tls_acceptor.clone();
                                let decorator = response_decorator.clone();
                                // Track the connection task so `delete` can drain it (issue #596).
                                imposter_clone.conn_tracker.spawn(async move {
                                    // Per-connection slot for a real transport fault (issue #239);
                                    // armed by the handler, applied by FaultIo on the response write.
                                    let fault_cell: FaultCell = Arc::new(Mutex::new(None));
                                    // FaultIo sits beneath TLS so #239 connection faults still break
                                    // an HTTPS connection.
                                    let faulted = FaultIo::new(stream, Arc::clone(&fault_cell));
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
                                                    TokioIo::new(tls),
                                                    imposter,
                                                    addr,
                                                    fault_cell,
                                                    conn_shutdown_rx,
                                                    port,
                                                    decorator,
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
                                                TokioIo::new(faulted),
                                                imposter,
                                                addr,
                                                fault_cell,
                                                conn_shutdown_rx,
                                                port,
                                                decorator,
                                            )
                                            .await
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Accept error on port {}: {}", port, e);
                            }
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        info!("Imposter on port {} shutting down", port);
                        break;
                    }
                }
            }
        });
        // Hand the accept-loop handle to the imposter so `delete` awaits the listener's teardown
        // (issue #596). Stored post-spawn because the loop needs the `Arc`-wrapped imposter.
        *imposter.serve_handle.lock() = Some(serve_handle);

        // Store imposter. Re-check the port under the write lock to close the TOCTOU between the
        // earlier read-only check and the bind: with SO_REUSEADDR/REUSEPORT two concurrent creates
        // for the same explicit port can both bind, so the loser of the insert race must stop the
        // listener it just started rather than leave an orphan accepting on the shared port.
        {
            let mut imposters = self.imposters.write();
            if imposters.contains_key(&port) {
                let _ = shutdown_tx.send(());
                return Err(ImposterError::PortInUse(port));
            }
            imposters.insert(port, Arc::clone(&imposter));
        }

        // Persist synchronously and surface the result: a create that can't be durably recorded
        // must fail loudly rather than return a success that vanishes on restart (issue #563).
        // On failure roll back the insert and stop the listener so `Err` keeps meaning "not
        // running" for every caller (create_for_apply / replace_imposter rely on that).
        if let Err(e) = self.persist_imposter_checked(&imposter).await {
            self.imposters.write().remove(&port);
            // A write that failed partway (e.g. ENOSPC) can leave a truncated {port}.json; drop it
            // so a later restart doesn't try to load a corrupt file for a create that never took.
            self.remove_persisted_imposter(port);
            let _ = shutdown_tx.send(());
            return Err(e);
        }

        Ok(port)
    }

    /// Bind to an available port for auto-assignment
    /// Starts from port 49152 (start of dynamic/private port range) and finds first available
    async fn find_available_port(&self, host: &str) -> Result<(u16, TcpListener), ImposterError> {
        let existing_ports: std::collections::HashSet<u16> = {
            let imposters = self.imposters.read();
            imposters.keys().copied().collect()
        };

        // Start from dynamic port range (49152-65535)
        // If we could allow random ports, rather than requiring the minimum available port,
        // we could bind to port 0, and let the OS pick an unused ephemeral port for us.
        // Try ports in this range until we find one that's available
        for port in 49152..=u16::MAX {
            if existing_ports.contains(&port) {
                continue;
            }
            // Try to bind to check if OS has it available
            match TcpListener::bind((host, port)).await {
                Ok(listener) => {
                    // Port is available, return the port and bound listener
                    return Ok((port, listener));
                }
                Err(_) => continue, // Port in use by OS, try next
            }
        }

        Err(ImposterError::BindError(
            0,
            "No available ports in range 49152-65535".to_string(),
        ))
    }

    /// Delete an imposter
    pub async fn delete_imposter(&self, port: u16) -> Result<ImposterConfig, ImposterError> {
        let config = self.delete_imposter_inner(port).await?;
        self.emit(ImposterEvent::Deleted(port));
        Ok(config)
    }

    /// Delete without emitting an event (see `create_imposter_inner`).
    async fn delete_imposter_inner(&self, port: u16) -> Result<ImposterConfig, ImposterError> {
        let imposter = {
            let mut imposters = self.imposters.write();
            imposters
                .remove(&port)
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
        let serve_handle = imposter.serve_handle.lock().take();
        if let Some(handle) = serve_handle
            && let Err(e) = handle.await
            && !e.is_cancelled()
        {
            warn!("imposter on port {port}: accept loop ended abnormally during delete: {e}");
        }
        // Drain in-flight connections within a bound. Each reacts to the signal above with a hyper
        // graceful shutdown (finish the in-flight response, then close), so this is normally
        // sub-millisecond; a genuinely stalled request (e.g. a long `wait`) is capped rather than
        // hanging the delete.
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
                warn!("failed to clear request journal for deleted imposter on port {port}: {e}");
            }
        }
        // Reclaim the shared proxy store's port slice so a later imposter reusing the port
        // doesn't inherit stale recordings (issue #315). The private default dies with the
        // imposter, so it needs no explicit clear.
        if let Some(store) = &self.proxy_store {
            store.clear(port);
        }

        info!("Imposter on port {} deleted", port);
        self.remove_persisted_imposter(port);
        Ok(imposter.config.clone())
    }

    /// Get an imposter by port
    pub fn get_imposter(&self, port: u16) -> Result<Arc<Imposter>, ImposterError> {
        let imposters = self.imposters.read();
        imposters
            .get(&port)
            .cloned()
            .ok_or(ImposterError::NotFound(port))
    }

    /// List all imposters
    pub fn list_imposters(&self) -> Vec<Arc<Imposter>> {
        let imposters = self.imposters.read();
        imposters.values().cloned().collect()
    }

    /// Delete all imposters. Emits a single `AllDeleted` event rather than one `Deleted`
    /// per port.
    pub async fn delete_all(&self) -> Vec<ImposterConfig> {
        let ports: Vec<u16> = {
            let imposters = self.imposters.read();
            imposters.keys().copied().collect()
        };

        let mut configs = Vec::new();
        for port in ports {
            match self.delete_imposter_inner(port).await {
                Ok(config) => configs.push(config),
                // Only realizable as a concurrent-delete race (NotFound) — already gone.
                Err(e) => debug!("delete_all: imposter on port {} not deleted: {}", port, e),
            }
        }

        self.emit(ImposterEvent::AllDeleted);
        configs
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
    pub async fn reload(&self, configs: Vec<ImposterConfig>) -> Result<(), ImposterError> {
        Self::validate_config_set(&configs)?;

        self.delete_all().await;
        for config in configs {
            self.create_imposter(config).await?;
        }
        Ok(())
    }

    /// Full-set validation shared by `reload` and `apply_config`: protocol validity, no
    /// duplicate explicit ports, and no duplicate explicit stub ids within an imposter
    /// (the invariant `add_stub_unique` enforces incrementally, issue #202 — duplicate ids
    /// would silently corrupt the stub-key diff). Runs before anything mutates.
    fn validate_config_set(configs: &[ImposterConfig]) -> Result<(), ImposterError> {
        let mut seen = std::collections::HashSet::new();
        for config in configs {
            match config.protocol.as_str() {
                "http" | "https" => {}
                other => return Err(ImposterError::InvalidProtocol(other.to_string())),
            }
            if let Some(port) = config.port
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
    /// set is patched in place and untouched slots keep their cycling state.
    ///
    /// The whole set is validated up front — `Err` means nothing was mutated. Per-port apply
    /// failures after that (e.g. a bind failure on a freed port) land in
    /// [`ApplyReport::failed`] while the remaining ports are still applied. Configs without
    /// an explicit port are never reconciled — each apply creates them fresh on an
    /// auto-assigned port (and reports their failures under port `0`).
    pub async fn apply_config(
        &self,
        desired: Vec<ImposterConfig>,
    ) -> Result<ApplyReport, ImposterError> {
        Self::validate_config_set(&desired)?;

        let mut report = ApplyReport::default();

        // Deletes first, so ports freed here can be re-bound by creates below.
        let desired_ports: std::collections::HashSet<u16> =
            desired.iter().filter_map(|c| c.port).collect();
        let removed_ports: Vec<u16> = {
            let imposters = self.imposters.read();
            let mut ports: Vec<u16> = imposters
                .keys()
                .copied()
                .filter(|port| !desired_ports.contains(port))
                .collect();
            ports.sort_unstable();
            ports
        };
        for port in removed_ports {
            match self.delete_imposter_inner(port).await {
                Ok(_) => {
                    report.deleted.push(port);
                    self.emit(ImposterEvent::Deleted(port));
                }
                Err(e) => report.failed.push((port, e)),
            }
        }

        for config in desired {
            let Some(port) = config.port else {
                // No explicit port → nothing to reconcile against; always an auto-assigned create.
                self.create_for_apply(config, 0, &mut report).await;
                continue;
            };

            let Ok(existing) = self.get_imposter(port) else {
                self.create_for_apply(config, port, &mut report).await;
                continue;
            };

            if imposter_level_differs(&existing.config, &config) {
                self.replace_imposter(port, config, &mut report).await;
                continue;
            }

            match existing.reconcile_stubs(config.stubs.clone()) {
                StubReconcile::Unchanged => {}
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
                    self.replace_imposter(port, config, &mut report).await;
                }
            }
        }

        Ok(report)
    }

    /// Create for `apply_config`: record the assigned port + Created event, or a failure
    /// under `fail_port` (the sentinel `0` for port-less, auto-assigned configs).
    async fn create_for_apply(
        &self,
        config: ImposterConfig,
        fail_port: u16,
        report: &mut ApplyReport,
    ) {
        match self.create_imposter_inner(config).await {
            Ok(assigned) => {
                report.created.push(assigned);
                self.emit(ImposterEvent::Created(assigned));
            }
            Err(e) => report.failed.push((fail_port, e)),
        }
    }

    /// Wholesale replace (PUT semantics): tear down, then recreate; all runtime state resets.
    /// When the recreate fails after a successful teardown, the imposter is genuinely gone —
    /// that is reported as deleted (list + event) alongside the failure, so listeners and the
    /// report never track a phantom imposter.
    async fn replace_imposter(&self, port: u16, config: ImposterConfig, report: &mut ApplyReport) {
        if let Err(e) = self.delete_imposter_inner(port).await {
            report.failed.push((port, e));
            return;
        }
        match self.create_imposter_inner(config).await {
            Ok(_) => {
                report.replaced.push(port);
                self.emit(ImposterEvent::Replaced(port));
            }
            Err(e) => {
                report.deleted.push(port);
                self.emit(ImposterEvent::Deleted(port));
                report.failed.push((port, e));
            }
        }
    }

    /// Get imposter count (for future metrics)
    pub fn count(&self) -> usize {
        self.imposters.read().len()
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
        imposter.replace_stub(index, stub)?;
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
        self.delete_all().await;
    }

    /// Persist an imposter's current config to datadir (if configured).
    /// Awaits the write and returns an error if it fails, so the caller can
    /// surface a 503 to the API client instead of silently losing the change.
    async fn persist_imposter_checked(&self, imposter: &Imposter) -> Result<(), ImposterError> {
        let Some(ref datadir) = self.datadir else {
            return Ok(());
        };
        let Some(port) = imposter.config.port else {
            return Ok(());
        };
        let mut snapshot = imposter.config.clone();
        snapshot.stubs = imposter.get_stubs();
        let path = datadir.join(format!("{port}.json"));
        let json = serde_json::to_string_pretty(&snapshot).map_err(|e| {
            ImposterError::PersistError(format!("Failed to serialize imposter {port}: {e}"))
        })?;
        tokio::fs::write(&path, json).await.map_err(|e| {
            ImposterError::PersistError(format!("Failed to write imposter {port} to {path:?}: {e}"))
        })
    }

    /// Remove an imposter's file from datadir (if configured).
    fn remove_persisted_imposter(&self, port: u16) {
        let Some(ref datadir) = self.datadir else {
            return;
        };
        let path = datadir.join(format!("{port}.json"));
        tokio::spawn(async move {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                // An absent file is the desired end state, not a failure: the imposter may never
                // have been persisted, or the file was already removed. Handling NotFound here
                // (rather than pre-checking `exists()`) also closes the TOCTOU window between
                // check and unlink.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => error!(
                    "Failed to remove persisted imposter {} at {:?}: {}",
                    port, path, e
                ),
            }
        });
    }
}

impl Default for ImposterManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Imposter-level diff for `apply_config`, comparing the configs with stubs stripped: any
/// difference (protocol, TLS, recordRequests, name, …) replaces wholesale. A serialization
/// failure on either side counts as "differs" — the conservative direction (worst case an
/// unnecessary replace, never a silently skipped change).
fn imposter_level_differs(a: &ImposterConfig, b: &ImposterConfig) -> bool {
    let flatten = |config: &ImposterConfig| {
        let mut flat = config.clone();
        flat.stubs = Vec::new();
        serde_json::to_value(&flat)
    };
    match (flatten(a), flatten(b)) {
        (Ok(va), Ok(vb)) => va != vb,
        (ra, rb) => {
            error!(
                "imposter config serialization failed during reconcile; treating as changed: {:?} {:?}",
                ra.err(),
                rb.err()
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_imposter_writes_to_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19501,
            "stubs": []
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let file = dir.path().join("19501.json");
        assert!(file.exists(), "imposter file should be written to datadir");

        let content = std::fs::read_to_string(&file).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["port"], 19501);
        assert_eq!(json["protocol"], "http");

        manager.delete_imposter(19501).await.unwrap();
    }

    #[tokio::test]
    async fn test_delete_imposter_removes_from_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19502,
            "stubs": []
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let file = dir.path().join("19502.json");
        assert!(file.exists(), "file should exist after create");

        manager.delete_imposter(19502).await.unwrap();
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

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19503,
            "stubs": []
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Remove the file out from under the manager, so the unlink races with an absent file.
        let file = dir.path().join("19503.json");
        std::fs::remove_file(&file).expect("pre-remove the persisted file");
        assert!(!file.exists(), "precondition: file is already gone");

        manager
            .delete_imposter(19503)
            .await
            .expect("delete must succeed when the datadir file is already gone");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(!file.exists(), "file must still be absent after delete");
    }

    #[tokio::test]
    async fn test_add_stub_updates_datadir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));

        let config = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19503,
            "stubs": []
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");
        // Wait for create_imposter's fire-and-forget persistence to land.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let stub: Stub = serde_json::from_value(serde_json::json!({
            "predicates": [],
            "responses": [{"is": {"statusCode": 200, "body": "hello"}}]
        }))
        .unwrap();

        manager.add_stub(19503, stub, None).await.unwrap();

        let file = dir.path().join("19503.json");
        let content = std::fs::read_to_string(&file).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(json["stubs"].as_array().unwrap().len(), 1);

        manager.delete_imposter(19503).await.unwrap();
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
            "port": 19600,
            "stubs": []
        }))
        .unwrap();

        manager
            .create_imposter(config)
            .await
            .expect("create should succeed with a writable datadir");

        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let stub: Stub = serde_json::from_value(serde_json::json!({
            "predicates": [],
            "responses": [{"is": {"statusCode": 200}}]
        }))
        .unwrap();

        let result = manager.add_stub(19600, stub, None).await;
        assert!(
            matches!(result, Err(ImposterError::PersistError(_))),
            "add_stub should return PersistError when datadir is not writable, got: {result:?}"
        );

        manager.delete_imposter(19600).await.unwrap();
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
            "port": 19611,
            "stubs": []
        }))
        .unwrap();

        let result = manager.create_imposter(config).await;
        assert!(
            matches!(result, Err(ImposterError::PersistError(_))),
            "create must surface PersistError when the datadir is not writable, got: {result:?}"
        );
        // Rolled back: the imposter must not linger in the map, and its port must be free again.
        assert!(
            manager.get_imposter(19611).is_err(),
            "a create that failed to persist must not leave a running imposter"
        );
        assert_eq!(manager.count(), 0, "the failed create must be rolled back");
    }

    #[tokio::test]
    async fn create_persists_file_synchronously() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = ImposterManager::with_datadir(Some(dir.path().to_path_buf()));
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19612,
            "stubs": []
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");
        // No sleep: create now awaits the persist, so the file exists the moment create returns.
        assert!(
            dir.path().join("19612.json").exists(),
            "create must have written the imposter file before returning"
        );

        manager.delete_imposter(19612).await.unwrap();
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
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19700,
            "stubs": [{
                "predicates": [{"equals": {"path": "/ping"}}],
                "responses": [{"is": {"statusCode": 200, "body": "pong"}}]
            }]
        }))
        .unwrap();

        manager.create_imposter(config).await.expect("create");

        // Open a keep-alive connection and confirm it is served.
        let mut stream = TcpStream::connect(("127.0.0.1", 19700))
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
        manager.delete_imposter(19700).await.expect("delete");

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
        match TcpStream::connect(("127.0.0.1", 19700)).await {
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

    // Issue #596: `delete` must await the in-flight generation's quiesce, not fire-and-forget. With
    // a request in flight behind a `wait` behavior, delete cannot return until that response has
    // been served and the connection drained — so a returning delete is proof the old generation is
    // gone. Deterministic: pre-fix `delete` returns in ~0ms; post-fix it blocks for the in-flight
    // response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_blocks_until_inflight_response_drains() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19701,
            "stubs": [{
                "predicates": [{"equals": {"path": "/slow"}}],
                "responses": [{"is": {"statusCode": 200, "body": "done"}, "_behaviors": {"wait": 400}}]
            }]
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        // Send a request that will spend ~400ms in the imposter's `wait` before its response is
        // written; don't read yet — the response is still in flight on this connection.
        let mut stream = TcpStream::connect(("127.0.0.1", 19701))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        // Let the request reach the server and enter the wait, well before its 400ms elapses.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(19701).await.expect("delete");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(250),
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
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19702,
            "stubs": [{
                "predicates": [{"equals": {"path": "/stall"}}],
                "responses": [{"is": {"statusCode": 200, "body": "eventually"}, "_behaviors": {"wait": 3000}}]
            }]
        }))
        .unwrap();
        manager.create_imposter(config).await.expect("create");

        let mut stream = TcpStream::connect(("127.0.0.1", 19702))
            .await
            .expect("connect");
        stream
            .write_all(b"GET /stall HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let started = std::time::Instant::now();
        manager.delete_imposter(19702).await.expect("delete");
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "delete must return within the drain bound (~150ms), not the full 3s stall; took {elapsed:?}"
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
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpStream;

        let manager = ImposterManager::new();
        let cfg = |body: &str| -> ImposterConfig {
            serde_json::from_value(serde_json::json!({
                "protocol": "http",
                "port": 19703,
                "stubs": [{
                    "predicates": [{"equals": {"path": "/g"}}],
                    "responses": [{"is": {"statusCode": 200, "body": body}}]
                }]
            }))
            .unwrap()
        };

        manager.create_imposter(cfg("old")).await.expect("create A");
        manager.delete_imposter(19703).await.expect("delete A");
        // delete returned only after A's listener was unbound, so this binds the same port cleanly.
        manager
            .create_imposter(cfg("new"))
            .await
            .expect("recreate B");

        let mut stream = TcpStream::connect(("127.0.0.1", 19703))
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

    // =========================================================================
    // Issue #316: incremental apply_config, move_stub, and imposter change events
    // =========================================================================

    use super::super::core::StubState;
    use super::super::reconcile::{ImposterEvent, ImposterEventListener};
    use super::super::types::RecordedRequest;
    use serde_json::json;

    fn imposter_cfg(v: serde_json::Value) -> ImposterConfig {
        serde_json::from_value(v).expect("test imposter config")
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
                "port": 0,
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
            request_from: "127.0.0.1".to_string(),
            method: "GET".to_string(),
            path: path.to_string(),
            query: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            body: None,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    // AC1: an unchanged imposter keeps recorded requests and cycler position
    // while a sibling port is modified.
    #[tokio::test]
    async fn apply_config_preserves_untouched_imposter_state() {
        let manager = ImposterManager::new();
        let p1 = imposter_cfg(json!({
            "protocol": "http", "port": 19410, "recordRequests": true,
            "stubs": [cycled_stub_json("a1", "a2"), stub_json("b"), stub_json("c")]
        }));
        let p2 = imposter_cfg(json!({
            "protocol": "http", "port": 19411,
            "stubs": [stub_json("x"), stub_json("y"), stub_json("z")]
        }));
        let report = manager
            .apply_config(vec![p1.clone(), p2])
            .await
            .expect("initial apply");
        assert_eq!(report.created, vec![19410, 19411]);
        assert!(report.failed.is_empty());

        let untouched = manager.get_imposter(19410).unwrap();
        untouched.record_request(recorded("/cycled"));
        assert_eq!(next_body(&untouched.stubs.load()[0]), "a1");

        let p2_changed = imposter_cfg(json!({
            "protocol": "http", "port": 19411,
            "stubs": [stub_json("x"), stub_json("y"), stub_json("z2")]
        }));
        let report = manager
            .apply_config(vec![p1, p2_changed])
            .await
            .expect("second apply");
        assert!(report.created.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.replaced.is_empty());
        assert_eq!(report.stub_patched, vec![19411]);

        let after = manager.get_imposter(19410).unwrap();
        assert!(
            Arc::ptr_eq(&untouched, &after),
            "untouched imposter must not be recreated"
        );
        assert_eq!(after.get_recorded_requests().len(), 1);
        assert_eq!(
            next_body(&after.stubs.load()[0]),
            "a2",
            "cycler position survives a sibling patch"
        );

        let patched = manager.get_imposter(19411).unwrap();
        let stubs = patched.get_stubs();
        let last = serde_json::to_value(&stubs[2]).unwrap();
        assert_eq!(last["responses"][0]["is"]["body"], "z2");

        manager.delete_all().await;
    }

    // AC2d: imposter-level field changes always replace wholesale.
    #[tokio::test]
    async fn apply_config_imposter_level_change_replaces_wholesale() {
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http", "port": 19420, "recordRequests": true,
            "stubs": [stub_json("a")]
        }));
        manager.apply_config(vec![initial]).await.expect("create");
        let before = manager.get_imposter(19420).unwrap();
        before.record_request(recorded("/a"));

        let renamed = imposter_cfg(json!({
            "protocol": "http", "port": 19420, "recordRequests": true, "name": "renamed",
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![renamed]).await.expect("apply");
        assert_eq!(report.replaced, vec![19420]);
        assert!(report.stub_patched.is_empty());

        let after = manager.get_imposter(19420).unwrap();
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
            "protocol": "http", "port": 19421,
            "stubs": [stub_json("a"), stub_json("b")]
        }));
        manager.apply_config(vec![initial]).await.expect("create");
        let before = manager.get_imposter(19421).unwrap();

        let rewritten = imposter_cfg(json!({
            "protocol": "http", "port": 19421,
            "stubs": [stub_json("x"), stub_json("y")]
        }));
        let report = manager.apply_config(vec![rewritten]).await.expect("apply");
        assert_eq!(report.replaced, vec![19421]);
        assert!(report.stub_patched.is_empty());
        let after = manager.get_imposter(19421).unwrap();
        assert!(!Arc::ptr_eq(&before, &after));

        manager.delete_all().await;
    }

    // AC6: full-set validation up front — nothing mutates on validation failure.
    #[tokio::test]
    async fn apply_config_validation_failure_mutates_nothing() {
        let manager = ImposterManager::new();
        let initial = imposter_cfg(json!({
            "protocol": "http", "port": 19422,
            "stubs": [stub_json("a")]
        }));
        manager.apply_config(vec![initial]).await.expect("create");
        let before = manager.get_imposter(19422).unwrap();

        let would_change = imposter_cfg(json!({
            "protocol": "http", "port": 19422,
            "stubs": [stub_json("x")]
        }));
        let invalid = imposter_cfg(json!({
            "protocol": "tcp", "port": 19423,
            "stubs": []
        }));
        let result = manager.apply_config(vec![would_change, invalid]).await;
        assert!(
            matches!(result, Err(ImposterError::InvalidProtocol(ref p)) if p == "tcp"),
            "got: {result:?}"
        );
        let after = manager.get_imposter(19422).unwrap();
        assert!(
            Arc::ptr_eq(&before, &after),
            "validation failure must not touch any imposter"
        );
        let stubs = after.get_stubs();
        assert_eq!(
            serde_json::to_value(&stubs[0]).unwrap()["responses"][0]["is"]["body"],
            "a"
        );
        assert!(manager.get_imposter(19423).is_err());

        let dup_a = imposter_cfg(json!({"protocol": "http", "port": 19424, "stubs": []}));
        let dup_b = imposter_cfg(json!({"protocol": "http", "port": 19424, "stubs": []}));
        let result = manager.apply_config(vec![dup_a, dup_b]).await;
        assert!(matches!(result, Err(ImposterError::PortInUse(19424))));
        assert!(manager.get_imposter(19424).is_err());

        manager.delete_all().await;
    }

    #[tokio::test]
    async fn apply_config_creates_new_and_deletes_missing_ports() {
        let manager = ImposterManager::new();
        let first = imposter_cfg(json!({"protocol": "http", "port": 19425, "stubs": []}));
        let report = manager.apply_config(vec![first]).await.expect("apply");
        assert_eq!(report.created, vec![19425]);

        let second = imposter_cfg(json!({"protocol": "http", "port": 19426, "stubs": []}));
        let report = manager.apply_config(vec![second]).await.expect("apply");
        assert_eq!(report.created, vec![19426]);
        assert_eq!(report.deleted, vec![19425]);
        assert!(manager.get_imposter(19425).is_err());
        assert!(manager.get_imposter(19426).is_ok());

        manager.delete_all().await;
    }

    #[derive(Default)]
    struct RecordingListener(Mutex<Vec<ImposterEvent>>);

    impl ImposterEventListener for RecordingListener {
        fn on_event(&self, event: &ImposterEvent) {
            self.0.lock().push(event.clone());
        }
    }

    // AC4: events fired per mutation kind.
    #[tokio::test]
    async fn events_fired_per_mutation_kind() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());

        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19430,
                "stubs": [stub_json("a"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create");
        let stub_d: Stub = serde_json::from_value(stub_json("d")).unwrap();
        manager.add_stub(19430, stub_d, None).await.expect("add");
        manager.move_stub(19430, 0, 1).await.expect("move");
        // live stubs now [b, a, c, d]; patch one of four (below the degenerate threshold)
        // and create a sibling in the same apply.
        let patched_cfg = imposter_cfg(json!({
            "protocol": "http", "port": 19430,
            "stubs": [stub_json("b"), stub_json("a"), stub_json("c"), stub_json("e")]
        }));
        let sibling = imposter_cfg(json!({"protocol": "http", "port": 19431, "stubs": []}));
        manager
            .apply_config(vec![patched_cfg, sibling.clone()])
            .await
            .expect("apply patch+create");
        // drop 19430, keep 19431 unchanged
        manager
            .apply_config(vec![sibling])
            .await
            .expect("apply delete");
        // imposter-level change on 19431
        let renamed = imposter_cfg(json!({
            "protocol": "http", "port": 19431, "name": "renamed", "stubs": []
        }));
        manager
            .apply_config(vec![renamed])
            .await
            .expect("apply replace");
        manager.delete_imposter(19431).await.expect("delete");
        manager.delete_all().await;

        let events = listener.0.lock().clone();
        assert_eq!(
            events,
            vec![
                ImposterEvent::Created(19430),
                ImposterEvent::StubsChanged(19430), // add_stub
                ImposterEvent::StubsChanged(19430), // move_stub
                ImposterEvent::StubsChanged(19430), // apply_config stub patch
                ImposterEvent::Created(19431),      // apply_config create
                ImposterEvent::Deleted(19430),      // apply_config delete
                ImposterEvent::Replaced(19431),     // apply_config imposter-level change
                ImposterEvent::Deleted(19431),      // delete_imposter
                ImposterEvent::AllDeleted,          // delete_all
            ]
        );
    }

    // Per-port apply failures land in `failed` while sibling ports still apply.
    #[tokio::test]
    async fn apply_config_partial_failure_reports_failed_port() {
        let manager = ImposterManager::new();
        let good = imposter_cfg(json!({"protocol": "http", "port": 19450, "stubs": []}));
        let bad_tls = imposter_cfg(json!({
            "protocol": "https", "port": 19451,
            "cert": "not a pem", "key": "not a pem",
            "stubs": []
        }));
        let report = manager
            .apply_config(vec![good, bad_tls])
            .await
            .expect("apply");
        assert_eq!(report.created, vec![19450], "sibling still applied");
        assert_eq!(report.failed.len(), 1);
        assert!(
            matches!(report.failed[0], (19451, ImposterError::Tls(_))),
            "got: {:?}",
            report.failed
        );
        assert!(manager.get_imposter(19450).is_ok());
        assert!(manager.get_imposter(19451).is_err());

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

        let cfg = imposter_cfg(json!({"protocol": "http", "port": 19454, "stubs": []}));
        let report = manager.apply_config(vec![cfg]).await.expect("apply");
        assert!(
            report.created.is_empty(),
            "a create that failed to persist must not appear as created: {:?}",
            report.created
        );
        assert!(
            matches!(report.failed[0], (19454, ImposterError::PersistError(_))),
            "got: {:?}",
            report.failed
        );
        assert!(manager.get_imposter(19454).is_err(), "must be rolled back");
        assert_eq!(manager.count(), 0);
    }

    // A replace whose recreate fails after teardown is honestly reported: the port lands in
    // both `deleted` and `failed`, and a Deleted event fires so listeners don't track a
    // phantom imposter.
    #[tokio::test]
    async fn apply_config_failed_replace_reports_deletion_and_event() {
        let listener = Arc::new(RecordingListener::default());
        let manager = ImposterManager::new().with_event_listener(listener.clone());
        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19452, "stubs": [stub_json("a")]
            })))
            .await
            .expect("create");

        // Imposter-level change (protocol + TLS) whose create fails: bad PEM material.
        let bad_tls = imposter_cfg(json!({
            "protocol": "https", "port": 19452,
            "cert": "not a pem", "key": "not a pem",
            "stubs": [stub_json("a")]
        }));
        let report = manager.apply_config(vec![bad_tls]).await.expect("apply");
        assert!(report.replaced.is_empty());
        assert_eq!(report.deleted, vec![19452], "teardown really happened");
        assert!(matches!(report.failed[0], (19452, ImposterError::Tls(_))));
        assert!(manager.get_imposter(19452).is_err());
        assert_eq!(
            listener.0.lock().clone(),
            vec![ImposterEvent::Created(19452), ImposterEvent::Deleted(19452),],
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
            "protocol": "http", "port": 19455, "stubs": [stub_a, stub_b]
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
        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19453,
                "stubs": [stub_json("a"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create succeeds with a writable datadir");
        // Break the datadir so the apply_config patch's persist fails (issue #173), while the
        // in-memory patch still stands.
        std::fs::remove_dir_all(&datadir).expect("break the datadir");

        let patched = imposter_cfg(json!({
            "protocol": "http", "port": 19453,
            "stubs": [stub_json("a"), stub_json("b"), stub_json("c2")]
        }));
        let report = manager.apply_config(vec![patched]).await.expect("apply");
        assert_eq!(report.stub_patched, vec![19453], "in-memory patch applied");
        assert!(
            matches!(report.failed[0], (19453, ImposterError::PersistError(_))),
            "persist failure must be observable, got: {:?}",
            report.failed
        );
        let stubs = manager.get_imposter(19453).unwrap().get_stubs();
        assert_eq!(
            serde_json::to_value(&stubs[2]).unwrap()["responses"][0]["is"]["body"],
            "c2"
        );

        manager.delete_all().await;
    }

    // move_stub is a positional move that preserves the moved slot's cycling state.
    #[tokio::test]
    async fn move_stub_repositions_and_preserves_cursor() {
        let manager = ImposterManager::new();
        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19440,
                "stubs": [cycled_stub_json("a1", "a2"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create");
        let imposter = manager.get_imposter(19440).unwrap();
        assert_eq!(next_body(&imposter.stubs.load()[0]), "a1");

        manager.move_stub(19440, 0, 2).await.expect("move");
        {
            let stubs = imposter.stubs.load();
            let first = serde_json::to_value(&stubs[0].stub).unwrap();
            assert_eq!(first["responses"][0]["is"]["body"], "b");
            assert_eq!(
                next_body(&stubs[2]),
                "a2",
                "moved stub keeps its cycling position"
            );
        }

        assert!(matches!(
            manager.move_stub(19440, 5, 0).await,
            Err(ImposterError::StubIndexOutOfBounds(5))
        ));
        assert!(matches!(
            manager.move_stub(19440, 0, 5).await,
            Err(ImposterError::StubIndexOutOfBounds(5))
        ));
        assert!(matches!(
            manager.move_stub(19441, 0, 0).await,
            Err(ImposterError::NotFound(19441))
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

            manager
                .create_imposter(imposter_cfg(
                    json!({"protocol": "http", "port": 19510, "stubs": []}),
                ))
                .await
                .expect("create");

            assert_eq!(*calls.lock(), 1, "provider consulted at construction");
            // Write through the provider's store handle, read via the imposter.
            store.set("f", "k", json!("v")).expect("set");
            let imposter = manager.get_imposter(19510).unwrap();
            assert_eq!(
                imposter.flow_get("f", "k").expect("get"),
                Some(json!("v")),
                "imposter must use the provider-supplied store"
            );

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

            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19511,
                    "stubs": [gated_stub()]
                })))
                .await
                .expect("create");

            assert_eq!(*calls.lock(), 1, "provider was consulted");
            // Built-in default for scenario-declaring stubs is a real in-memory store.
            let imposter = manager.get_imposter(19511).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19512,
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/plain"}}],
                        "responses": [{"is": {"statusCode": 200}}]
                    }]
                })))
                .await
                .expect("create");

            let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
            manager
                .add_stub(19512, stub.clone(), None)
                .await
                .expect("add");

            let imposter = manager.get_imposter(19512).unwrap();
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

        // AC4: with no provider, a NoOp-eligible plain imposter is still NoOp — the caveat
        // stands without a provider, i.e. behavior is byte-for-byte unchanged.
        #[tokio::test]
        async fn no_provider_leaves_late_added_scenario_stuck() {
            let manager = ImposterManager::new();
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19513,
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/plain"}}],
                        "responses": [{"is": {"statusCode": 200}}]
                    }]
                })))
                .await
                .expect("create");

            let stub: Stub = serde_json::from_value(gated_stub()).expect("stub");
            manager
                .add_stub(19513, stub.clone(), None)
                .await
                .expect("add");

            let imposter = manager.get_imposter(19513).unwrap();
            imposter
                .apply_scenario_transition("f", &stub)
                .expect("transition");
            assert_eq!(
                imposter.scenario_state("f", "order").expect("state"),
                "Started",
                "without a provider the NoOp caveat is preserved (unchanged behavior)"
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

            manager
                .create_imposter(imposter_cfg(
                    json!({"protocol": "http", "port": 19514, "stubs": []}),
                ))
                .await
                .expect("create");

            assert_eq!(
                *seen_port.lock(),
                Some(19514),
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

            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19515,
                    "_rift": {"flowState": {"backend": "inmemory", "ttlSeconds": 60}},
                    "stubs": []
                })))
                .await
                .expect("create");

            // The provider's own store handle observes the imposter's writes → the provider
            // store is in use, not the built-in one the explicit flowState would have made.
            store.set("f", "k", json!("provider")).expect("set");
            let imposter = manager.get_imposter(19515).unwrap();
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

            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19516,
                    "_rift": {"flowState": {"backend": "inmemory", "ttlSeconds": 60}},
                    "stubs": []
                })))
                .await
                .expect("create");

            let imposter = manager.get_imposter(19516).unwrap();
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
        let manager = ImposterManager::new();
        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19560,
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

        let mut handles = Vec::new();
        for _ in 0..100 {
            handles.push(tokio::spawn(async {
                let resp = reqwest::get("http://127.0.0.1:19560/m")
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

        // AC2: an injected sequencer receives the correct key parts and repeats, and its
        // decisions drive the served responses (repeat honored through the real pipeline).
        #[tokio::test]
        async fn sequencer_receives_keys_and_drives_cycling() {
            let (recorder, manager) = manager_with_recorder();
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19520,
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let mut bodies = Vec::new();
            for _ in 0..3 {
                bodies.push(
                    reqwest::get("http://127.0.0.1:19520/cycle")
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
            assert_eq!(port, 19520);
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19521,
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let _ = reqwest::get("http://127.0.0.1:19521/cycle")
                .await
                .expect("request");
            let slot_before = recorder.nexts.lock().last().expect("recorded").1;

            let replacement: Stub = serde_json::from_value(cycling_stub_json()).expect("stub");
            manager
                .replace_stub_by_id(19521, "s1", replacement)
                .await
                .expect("replace");

            let _ = reqwest::get("http://127.0.0.1:19521/cycle")
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19522,
                    "stubs": [cycling_stub_json(), stub_json("other")]
                })))
                .await
                .expect("create");

            manager
                .delete_stub_by_id(19522, "s1")
                .await
                .expect("delete by id");
            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(19522, Some("s1".to_string()))),
                "stub delete resets that stub's cursors: {:?}",
                recorder.resets.lock()
            );

            let fresh: Vec<Stub> = vec![serde_json::from_value(stub_json("fresh")).expect("stub")];
            manager
                .replace_stubs(19522, fresh)
                .await
                .expect("replace all");
            assert!(
                recorder.resets.lock().contains(&(19522, None)),
                "bulk replace resets the whole port"
            );

            recorder.resets.lock().clear();
            manager.delete_imposter(19522).await.expect("teardown");
            assert!(
                recorder.resets.lock().contains(&(19522, None)),
                "imposter teardown is the port-wide GC hook"
            );
        }

        // A failing sequencer surfaces as the structured backend error (#318), never a
        // silent wrong response.
        #[tokio::test]
        async fn failing_sequencer_surfaces_structured_503() {
            let manager = ImposterManager::new()
                .with_sequencer(Arc::new(FailingSequencer) as Arc<dyn ResponseSequencer>);
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19523,
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let resp = reqwest::get("http://127.0.0.1:19523/cycle")
                .await
                .expect("request");
            assert_eq!(resp.status(), 503, "sequencer outage is a structured 503");
            let body: serde_json::Value = resp.json().await.expect("json");
            assert_eq!(body["error"], "backendUnavailable");
            assert_eq!(body["feature"], "sequencer");

            manager.delete_all().await;
        }

        // Scope (issue #223 space) is carried in the key: a space-scoped stub reports its
        // space, exercised directly at the imposter layer (the HTTP gate needs a matching
        // flow id, which is orthogonal here).
        #[tokio::test]
        async fn space_scoped_stub_reports_scope() {
            let (recorder, manager) = manager_with_recorder();
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19524,
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
            manager.add_stub(19524, spaced, None).await.expect("add");

            let imposter = manager.get_imposter(19524).unwrap();
            let stub_state = imposter.stubs.load()[0].clone();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19527,
                    "stubs": [cycling_stub_json()]
                })))
                .await
                .expect("create");

            let resp = reqwest::get("http://127.0.0.1:19527/cycle")
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19525,
                    "stubs": [{
                        "predicates": [{"equals": {"path": "/anon"}}],
                        "responses": [{"is": {"statusCode": 200, "body": "anon"}}]
                    }]
                })))
                .await
                .expect("create");

            let _ = reqwest::get("http://127.0.0.1:19525/anon")
                .await
                .expect("request");
            let key_used = recorder.nexts.lock().last().expect("recorded").2.clone();
            assert!(
                key_used.starts_with('~') && key_used.ends_with("#0"),
                "id-less stub keys as a content key, got {key_used}"
            );

            manager
                .delete_stub(19525, 0)
                .await
                .expect("delete by index");
            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(19525, Some(key_used.clone()))),
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
                "protocol": "http", "port": 19526,
                "stubs": [
                    cycling_stub_json(),
                    {"id": "doomed",
                     "predicates": [{"equals": {"path": "/doomed"}}],
                     "responses": [{"is": {"statusCode": 200, "body": "bye"}}]}
                ]
            }));
            manager
                .apply_config(vec![initial.clone()])
                .await
                .expect("create via apply");

            let _ = reqwest::get("http://127.0.0.1:19526/cycle")
                .await
                .expect("request");
            let slot_before = recorder.nexts.lock().last().expect("recorded").1;

            let mut patched = initial;
            patched.stubs.truncate(1);
            manager.apply_config(vec![patched]).await.expect("patch");

            assert!(
                recorder
                    .resets
                    .lock()
                    .contains(&(19526, Some("doomed".to_string()))),
                "apply_config removal fires the per-stub GC hook: {:?}",
                recorder.resets.lock()
            );

            let _ = reqwest::get("http://127.0.0.1:19526/cycle")
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19530, "recordRequests": false,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get("http://127.0.0.1:19530/x")
                .await
                .expect("request");
            assert_eq!(*journal.notes.lock(), vec![19530]);
            assert!(journal.records.lock().is_empty(), "recording is off");
            let imposter = manager.get_imposter(19530).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19531, "recordRequests": true,
                    "_rift": { "flowState": { "flowIdSource": "header:X-Flow-Id" } },
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let client = reqwest::Client::new();
            let _ = client
                .get("http://127.0.0.1:19531/x")
                .header("X-Flow-Id", "flow-42")
                .send()
                .await
                .expect("request");
            let records = journal.records.lock().clone();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, 19531);
            assert_eq!(records[0].1, "flow-42", "flow id resolved per flowIdSource");

            // No header → falls back to the imposter port, matching resolve semantics.
            let _ = client
                .get("http://127.0.0.1:19531/y")
                .send()
                .await
                .expect("request");
            assert_eq!(journal.records.lock().last().expect("recorded").1, "19531");

            manager.delete_all().await;
        }

        // AC2: admin-facing imposter methods route through the injected journal.
        #[tokio::test]
        async fn admin_paths_route_through_journal() {
            let (journal, manager) = manager_with_journal();
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19532, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get("http://127.0.0.1:19532/seen")
                .await
                .expect("request");
            let imposter = manager.get_imposter(19532).unwrap();
            let recorded = imposter.get_recorded_requests();
            assert_eq!(recorded.len(), 1, "GET requests reads via the journal");
            assert_eq!(recorded[0].path, "/seen");

            imposter.retain_recorded_requests(|r| r.path != "/seen");
            assert_eq!(*journal.retains.lock(), vec![19532]);
            assert!(imposter.get_recorded_requests().is_empty());
            assert_eq!(imposter.get_request_count(), 1, "retain keeps the count");

            imposter.clear_recorded_requests().expect("clear");
            assert!(journal.clears.lock().contains(&19532));
            assert_eq!(imposter.get_request_count(), 0, "clear resets the count");

            manager.delete_all().await;
        }

        // AC1/AC2: teardown_space clears exactly one correlated slice via clear_flow.
        #[tokio::test]
        async fn teardown_space_uses_clear_flow() {
            let (journal, manager) = manager_with_journal();
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19533, "recordRequests": true,
                    "_rift": { "flowState": { "flowIdSource": "header:X-Flow-Id" } },
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let client = reqwest::Client::new();
            for flow in ["sp-1", "sp-2"] {
                let _ = client
                    .get("http://127.0.0.1:19533/x")
                    .header("X-Flow-Id", flow)
                    .send()
                    .await
                    .expect("request");
            }

            let imposter = manager.get_imposter(19533).unwrap();
            imposter.teardown_space("sp-1").expect("teardown");
            assert!(
                journal
                    .flow_clears
                    .lock()
                    .contains(&(19533, "sp-1".to_string())),
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19534, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let _ = reqwest::get("http://127.0.0.1:19534/x")
                .await
                .expect("request");

            manager.delete_imposter(19534).await.expect("delete");
            assert!(journal.clears.lock().contains(&19534));
            assert_eq!(journal.inner.count(19534), 0);
            assert!(journal.inner.read(19534).entries.is_empty());
        }

        // Two live imposters sharing one injected journal stay isolated through the
        // imposter wrappers: clear on port A leaves port B's entries and count intact.
        #[tokio::test]
        async fn shared_journal_isolates_ports_through_wrappers() {
            let (_journal, manager) = manager_with_journal();
            for port in [19536u16, 19537] {
                manager
                    .create_imposter(imposter_cfg(json!({
                        "protocol": "http", "port": port, "recordRequests": true,
                        "stubs": [stub_json("ok")]
                    })))
                    .await
                    .expect("create");
                let _ = reqwest::get(format!("http://127.0.0.1:{port}/x"))
                    .await
                    .expect("request");
            }

            let a = manager.get_imposter(19536).unwrap();
            let b = manager.get_imposter(19537).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19535, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");

            let _ = reqwest::get("http://127.0.0.1:19535/x")
                .await
                .expect("request");
            let imposter = manager.get_imposter(19535).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19538, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let imposter = manager.get_imposter(19538).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19539, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            let imposter = manager.get_imposter(19539).unwrap();
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
            manager
                .create_imposter(imposter_cfg(json!({
                    "protocol": "http", "port": 19540, "recordRequests": true,
                    "stubs": [stub_json("ok")]
                })))
                .await
                .expect("create");
            manager
                .delete_imposter(19540)
                .await
                .expect("delete succeeds despite a failed GC clear");
        }
    }

    // =========================================================================
    // Issue #315: pluggable ProxyRecordingStore
    // =========================================================================
    mod proxy_store {
        use super::*;
        use crate::recording::{
            ClaimOutcome, ClaimToken, LocalProxyStore, ProxyMode, ProxyRecordingStore,
            ProxyStoreError, RecordedResponse, RequestSignature,
        };

        /// Delegates to a LocalProxyStore while recording every claim/release/clear it sees.
        /// `fail_claim`/`fail_record` inject the `Err` paths a real external backend can take,
        /// which the built-in store never exercises.
        struct SpyProxyStore {
            inner: LocalProxyStore,
            fail_claim: bool,
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
                    inner: LocalProxyStore::new(ProxyMode::ProxyOnce),
                    fail_claim,
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
                self.claims.lock().push(port);
                if self.fail_claim {
                    return Err(ProxyStoreError::Unavailable("spy".into()));
                }
                self.inner.try_claim(port, sig)
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

        /// A port with nothing listening: bind an ephemeral port to learn a free number, then drop
        /// the listener so a connection there is refused — deterministic, unlike a guessed literal.
        fn closed_port() -> u16 {
            std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind ephemeral")
                .local_addr()
                .expect("local_addr")
                .port()
        }

        // AC7: a shared proxy store is injected into imposters, keyed by port, exercised on the
        // proxy hot path, and cleared on imposter deletion.
        #[tokio::test]
        async fn shared_store_is_used_and_cleared_on_delete() {
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
            let spy = Arc::new(SpyProxyStore::new());
            let manager = ImposterManager::new()
                .with_proxy_store(spy.clone() as Arc<dyn ProxyRecordingStore>);
            // Forward to a closed port so the upstream leg always fails.
            let dead = closed_port();
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
    }
}
