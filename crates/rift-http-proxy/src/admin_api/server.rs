//! Admin API server.

use crate::admin_api::router::route_request;
use crate::imposter::ImposterManager;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Admin API server for Rift
pub struct AdminApiServer {
    addr: SocketAddr,
    manager: Arc<ImposterManager>,
    api_key: Option<Arc<String>>,
}

impl AdminApiServer {
    /// Create a new admin API server
    pub fn new(addr: SocketAddr, manager: Arc<ImposterManager>, api_key: Option<String>) -> Self {
        Self {
            addr,
            manager,
            api_key: api_key.map(Arc::new),
        }
    }

    /// Run the admin API server
    pub async fn run(self) -> Result<(), anyhow::Error> {
        let listener = TcpListener::bind(self.addr).await?;
        info!(
            "Rift Admin API (Mountebank-compatible) listening on http://{}",
            self.addr
        );

        if self.api_key.is_some() {
            info!("Admin API authentication enabled (--apikey)");
        }

        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let manager = Arc::clone(&self.manager);
            let api_key = self.api_key.clone();

            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let manager = Arc::clone(&manager);
                    let api_key = api_key.clone();
                    async move {
                        if let Some(ref key) = api_key {
                            let auth = req
                                .headers()
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("");
                            if auth != key.as_str() {
                                return Ok::<_, hyper::Error>(unauthorized_response());
                            }
                        }
                        route_request(req, manager).await
                    }
                });

                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    debug!("Admin API connection error: {}", e);
                }
            });
        }
    }
}

fn unauthorized_response() -> Response<Full<Bytes>> {
    let body = r#"{"errors":[{"code":"unauthorized","message":"Invalid authorization token"}]}"#;
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("Content-Type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("infallible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unauthorized_response_status() {
        let resp = unauthorized_response();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_unauthorized_response_body() {
        use http_body_util::BodyExt;
        let resp = unauthorized_response();
        let body_bytes = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(resp.into_body().collect())
            .unwrap()
            .to_bytes();
        let body_str = std::str::from_utf8(&body_bytes).unwrap();
        let json: serde_json::Value = serde_json::from_str(body_str).unwrap();
        assert_eq!(json["errors"][0]["code"], "unauthorized");
        assert!(!json["errors"][0]["message"].as_str().unwrap().is_empty());
    }

    #[test]
    fn test_admin_server_new_with_api_key() {
        let manager = Arc::new(ImposterManager::new());
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server = AdminApiServer::new(addr, manager, Some("secret".to_string()));
        assert!(server.api_key.is_some());
        assert_eq!(server.api_key.unwrap().as_str(), "secret");
    }

    #[test]
    fn test_admin_server_new_without_api_key() {
        let manager = Arc::new(ImposterManager::new());
        let addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let server = AdminApiServer::new(addr, manager, None);
        assert!(server.api_key.is_none());
    }
}
