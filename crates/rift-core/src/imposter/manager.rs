//! ImposterManager - lifecycle management for multiple imposters.
//!
//! This module handles creating, deleting, and managing multiple imposters,
//! each running on its own port.

use super::core::Imposter;
use super::handler::handle_imposter_request;
use super::types::{ImposterConfig, ImposterError, Stub};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{debug, error, info};

/// Manages the lifecycle of multiple imposters
pub struct ImposterManager {
    /// Active imposters by port
    imposters: RwLock<HashMap<u16, Arc<Imposter>>>,
    /// Global shutdown signal (for future graceful shutdown)
    shutdown_tx: broadcast::Sender<()>,
    /// Optional data directory for persistence write-through
    datadir: Option<Arc<PathBuf>>,
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
        }
    }

    /// Create and start an imposter
    /// Returns the assigned port (which may have been auto-assigned if not specified)
    pub async fn create_imposter(&self, mut config: ImposterConfig) -> Result<u16, ImposterError> {
        // Validate protocol first
        match config.protocol.as_str() {
            "http" | "https" => {}
            proto => return Err(ImposterError::InvalidProtocol(proto.to_string())),
        }

        let bind_host: &str = config.host.as_deref().unwrap_or("0.0.0.0");
        // Determine port - either from config or auto-assign
        let (port, listener) = if let Some(p) = config.port {
            // Check if specified port is already in use
            if self.imposters.read().contains_key(&p) {
                return Err(ImposterError::PortInUse(p));
            }
            (
                p,
                TcpListener::bind((bind_host, p))
                    .await
                    .map_err(|e| ImposterError::BindError(p, e.to_string()))?,
            )
        } else {
            // Auto-assign port: find an available port starting from a base
            self.find_available_port(bind_host).await?
        };

        config.port = Some(port);

        info!("Imposter bound to {}:{}", bind_host, port);
        // Create imposter
        let mut imposter = Imposter::new(config);

        // Create shutdown channel for this imposter
        let (shutdown_tx, _) = broadcast::channel(1);
        imposter.shutdown_tx = Some(shutdown_tx.clone());

        let imposter = Arc::new(imposter);

        // Start serving
        let imposter_clone = Arc::clone(&imposter);
        let conn_shutdown_tx = shutdown_tx.clone();
        let mut shutdown_rx = shutdown_tx.subscribe();

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
                                let mut conn_shutdown_rx = conn_shutdown_tx.subscribe();
                                tokio::spawn(async move {
                                    let io = TokioIo::new(stream);
                                    let service = service_fn(move |req| {
                                        let imposter = Arc::clone(&imposter);
                                        async move {
                                            handle_imposter_request(req, imposter, addr).await
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
                                            // Stop accepting new requests on this connection and
                                            // close it once any in-flight request completes.
                                            conn.as_mut().graceful_shutdown();
                                            if let Err(e) = conn.as_mut().await {
                                                debug!(
                                                    "Connection error on port {} during shutdown: {}",
                                                    port, e
                                                );
                                            }
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

        // Store imposter
        {
            let mut imposters = self.imposters.write();
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

    /// Delete all imposters
    pub async fn delete_all(&self) -> Vec<ImposterConfig> {
        let ports: Vec<u16> = {
            let imposters = self.imposters.read();
            imposters.keys().copied().collect()
        };

        let mut configs = Vec::new();
        for port in ports {
            if let Ok(config) = self.delete_imposter(port).await {
                configs.push(config);
            }
        }

        configs
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
        self.persist_imposter_checked(&imposter).await
    }

    /// Delete the stub addressed by `id` (issue #202).
    pub async fn delete_stub_by_id(&self, port: u16, id: &str) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        if !imposter.delete_stub_by_id(id) {
            return Err(ImposterError::StubNotFound(id.to_string()));
        }
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
        imposter.teardown_space(space);
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
        imposter
            .replace_stub(index, stub)
            .map_err(|_| ImposterError::StubIndexOutOfBounds(index))?;
        self.persist_imposter_checked(&imposter).await
    }

    /// Delete a stub
    pub async fn delete_stub(&self, port: u16, index: usize) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter
            .delete_stub(index)
            .map_err(|_| ImposterError::StubIndexOutOfBounds(index))?;
        self.persist_imposter_checked(&imposter).await
    }

    /// Replace all stubs for an imposter
    pub async fn replace_stubs(&self, port: u16, stubs: Vec<Stub>) -> Result<(), ImposterError> {
        let imposter = self.get_imposter(port)?;
        imposter.replace_stubs(stubs);
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
        let port = match imposter.config.port {
            Some(p) => p,
            None => return Ok(()),
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
        let port = match imposter.config.port {
            Some(p) => p,
            None => return,
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
            if path.exists() {
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    error!(
                        "Failed to remove persisted imposter {} at {:?}: {}",
                        port, path, e
                    );
                }
            }
        });
    }
}

impl Default for ImposterManager {
    fn default() -> Self {
        Self::new()
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
}
