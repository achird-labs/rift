//! ImposterManager - lifecycle management for multiple imposters.
//!
//! This module handles creating, deleting, and managing multiple imposters,
//! each running on its own port.

use super::core::Imposter;
use super::fault_io::{FaultCell, FaultIo, TcpFaultKind};
use super::handler::handle_imposter_request_decorated;
use super::reconcile::{ApplyReport, ImposterEvent, ImposterEventListener, StubReconcile};
use super::types::{ImposterConfig, ImposterError, Stub};
use crate::extensions::decorate::ResponseDecorator;
use crate::extensions::flow_state::FlowStoreProvider;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

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

/// Serve one HTTP/1 connection (over plaintext or an already-handshaked TLS stream) until it
/// completes or the imposter is torn down. Shared by the plain and HTTPS serve paths (issue #206).
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
    let conn = http1::Builder::new().serve_connection(io, service);
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
}

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
        }
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

    fn emit(&self, event: ImposterEvent) {
        if let Some(listener) = &self.event_listener {
            listener.on_event(&event);
        }
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
        let mut imposter = Imposter::new_with_provider(config, self.flow_store_provider.as_ref());

        // Create shutdown channel for this imposter
        let (shutdown_tx, _) = broadcast::channel(1);
        imposter.shutdown_tx = Some(shutdown_tx.clone());

        let imposter = Arc::new(imposter);

        // Start serving
        let imposter_clone = Arc::clone(&imposter);
        let conn_shutdown_tx = shutdown_tx.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();
        let response_decorator = self.response_decorator.clone();

        let _handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                let imposter = Arc::clone(&imposter_clone);
                                // Each connection watches the shutdown signal so existing
                                // keep-alive connections are gracefully closed on delete,
                                // not just new connections (issue #207).
                                let conn_shutdown_rx = conn_shutdown_tx.subscribe();
                                // Per-imposter TLS acceptor is cheap to clone (Arc-backed).
                                let tls_acceptor = tls_acceptor.clone();
                                let decorator = response_decorator.clone();
                                tokio::spawn(async move {
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

        self.persist_imposter(&imposter);

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

        // Send shutdown signal
        if let Some(ref tx) = imposter.shutdown_tx {
            let _ = tx.send(());
        }

        // Clear JavaScript inject state for this imposter
        #[cfg(feature = "javascript")]
        crate::scripting::clear_imposter_state(port);

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
                StubReconcile::Patched => {
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
        imposter.delete_stub(index)?;
        self.emit(ImposterEvent::StubsChanged(port));
        self.persist_imposter_checked(&imposter).await
    }

    /// Replace all stubs for an imposter
    pub async fn replace_stubs(&self, port: u16, stubs: Vec<Stub>) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.replace_stubs(stubs);
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

    /// Persist an imposter's current config to datadir (if configured).
    /// Fire-and-forget: write failures are logged but not propagated.
    /// Used by create_imposter where the imposter is already running and
    /// a persistence failure should not roll back the in-memory state.
    fn persist_imposter(&self, imposter: &Imposter) {
        let Some(ref datadir) = self.datadir else {
            return;
        };
        let Some(port) = imposter.config.port else {
            return;
        };
        let mut snapshot = imposter.config.clone();
        snapshot.stubs = imposter.get_stubs();
        let path = datadir.join(format!("{port}.json"));
        tokio::spawn(async move {
            match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => {
                    if let Err(e) = tokio::fs::write(&path, json).await {
                        error!("Failed to persist imposter {} to {:?}: {}", port, path, e);
                    }
                }
                Err(e) => error!(
                    "Failed to serialize imposter {} for persistence: {}",
                    port, e
                ),
            }
        });
    }

    /// Remove an imposter's file from datadir (if configured).
    fn remove_persisted_imposter(&self, port: u16) {
        let Some(ref datadir) = self.datadir else {
            return;
        };
        let path = datadir.join(format!("{port}.json"));
        tokio::spawn(async move {
            if path.exists()
                && let Err(e) = tokio::fs::remove_file(&path).await
            {
                error!(
                    "Failed to remove persisted imposter {} at {:?}: {}",
                    port, path, e
                );
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
        // Point the datadir at a path that cannot be written (a file, not a dir).
        // The write will fail, and add_stub must propagate ImposterError::PersistError.
        let fake_dir = tempfile::tempdir().expect("tempdir");
        // Use a datadir sub-path that was never created, so fs::write fails.
        let nonexistent_datadir = fake_dir.path().join("does_not_exist_subdir");

        let manager = ImposterManager::with_datadir(Some(nonexistent_datadir));
        let config: ImposterConfig = serde_json::from_value(serde_json::json!({
            "protocol": "http",
            "port": 19600,
            "stubs": []
        }))
        .unwrap();

        manager
            .create_imposter(config)
            .await
            .expect("create should succeed in memory");

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

        // Delete the imposter, then give the per-connection graceful shutdown a
        // moment to land on the idle keep-alive socket (heuristic wait — the
        // broadcast send is synchronous and idle-keepalive close is near-instant).
        manager.delete_imposter(19700).await.expect("delete");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

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
        untouched.record_request(&recorded("/cycled"));
        assert_eq!(next_body(&untouched.stubs.read()[0]), "a1");

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
            next_body(&after.stubs.read()[0]),
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
        before.record_request(&recorded("/a"));

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
            "protocol": "http", "port": 19454, "stubs": [stub_a, stub_b]
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
        let manager = ImposterManager::with_datadir(Some(dir.path().join("does_not_exist_subdir")));
        manager
            .create_imposter(imposter_cfg(json!({
                "protocol": "http", "port": 19453,
                "stubs": [stub_json("a"), stub_json("b"), stub_json("c")]
            })))
            .await
            .expect("create succeeds in memory");

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
        assert_eq!(next_body(&imposter.stubs.read()[0]), "a1");

        manager.move_stub(19440, 0, 2).await.expect("move");
        {
            let stubs = imposter.stubs.read();
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
}
