//! `rift healthcheck` (issue #664): probe a running server's admin API and exit non-zero when it
//! is not healthy.
//!
//! This lives in the binary because the image has nothing else to probe with: the `-static` flavor
//! is `FROM scratch`, so there is no shell and no curl — the container HEALTHCHECK can only exec
//! the rift binary itself. Dropping curl is the point (CVE-2025-10148 landed in the image purely
//! to serve a HEALTHCHECK line).
//!
//! Kept as plain, testable library functions — `main.rs`/[`dispatch`] are thin CLI wrappers (URL
//! construction, printing, exit codes) around [`probe`], which tests call directly.

use anyhow::{Context, Result, bail};
use std::time::Duration;

/// Probe `url`, succeeding only when it answers 2xx.
///
/// A non-2xx answer and a transport failure are both `Err`: for a liveness probe "the server told
/// me it is broken" and "the server did not answer" are the same verdict.
pub async fn probe(url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build the healthcheck client")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("healthcheck request to {url} failed"))?;

    let status = response.status();
    if !status.is_success() {
        bail!("healthcheck: {url} answered {status}");
    }

    Ok(())
}

/// The admin API health endpoint to probe, derived from the server's own `--host`/`--port`.
pub fn default_url(host: &str, port: u16) -> String {
    // `--host`/`MB_HOST` is a *bind* address: 0.0.0.0 (and ::) mean "listen on every interface",
    // which is not a meaningful address to connect *to*. A server bound that way answers on
    // loopback, which is where a probe running inside the same container should knock.
    let host = match host {
        "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1",
        h => h,
    };

    // A bare IPv6 literal has to be bracketed in a URL authority, or the port reads as another hextet.
    if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{port}/health")
    } else {
        format!("http://{host}:{port}/health")
    }
}

/// `rift healthcheck`: probe the admin API. `Err` is the unhealthy verdict — `main` turns it into
/// the non-zero exit Docker reads, and prints the cause for whoever runs `docker inspect`.
///
/// Runs on its own runtime and never touches the server bootstrap — a probe must not write the
/// running server's PID file or log files out from under it.
pub fn dispatch(url: Option<String>, host: &str, port: u16, timeout_secs: u64) -> Result<()> {
    let url = url.unwrap_or_else(|| default_url(host, port));
    // One outbound GET; a current-thread runtime is enough (and is what `script_cli` uses).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the probe runtime")?;

    runtime.block_on(probe(&url, Duration::from_secs(timeout_secs)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve `status_line` to every caller until dropped. Returns the bound address.
    async fn spawn_server(status_line: &'static str) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = r#"{"status":"ok"}"#;
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        addr
    }

    /// Accept connections but never answer, so a probe can only end by timing out.
    async fn spawn_hung_server() -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock);
            }
        });
        addr
    }

    #[tokio::test]
    async fn probe_accepts_2xx() {
        let addr = spawn_server("200 OK").await;
        let url = format!("http://{addr}/health");
        assert!(probe(&url, Duration::from_secs(5)).await.is_ok());
    }

    #[tokio::test]
    async fn probe_rejects_5xx() {
        let addr = spawn_server("500 Internal Server Error").await;
        let url = format!("http://{addr}/health");
        let err = probe(&url, Duration::from_secs(5))
            .await
            .expect_err("a 500 is not healthy");
        assert!(
            err.to_string().contains("500"),
            "error should name the status: {err}"
        );
    }

    #[tokio::test]
    async fn probe_rejects_4xx() {
        // A reachable server with no /health route is still not a healthy rift.
        let addr = spawn_server("404 Not Found").await;
        let url = format!("http://{addr}/health");
        assert!(probe(&url, Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn probe_reports_connection_refused() {
        // Bind then drop: the port is now almost certainly free, so connecting is refused.
        let addr = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("local_addr")
        };
        let url = format!("http://{addr}/health");
        assert!(probe(&url, Duration::from_secs(5)).await.is_err());
    }

    #[tokio::test]
    async fn probe_times_out_on_a_hung_server() {
        // Without a timeout the probe would hang here forever and only Docker's own --timeout
        // would end it; the exit code has to come from us.
        let addr = spawn_hung_server().await;
        let url = format!("http://{addr}/health");
        assert!(probe(&url, Duration::from_millis(200)).await.is_err());
    }

    #[test]
    fn default_url_maps_bind_any_to_loopback() {
        for bind_any in ["0.0.0.0", "::", "[::]", ""] {
            assert_eq!(
                default_url(bind_any, 2525),
                "http://127.0.0.1:2525/health",
                "{bind_any} is a bind address, not a connect address"
            );
        }
    }

    // `dispatch` is the whole CLI path below arg parsing: it must turn a healthy server into Ok and
    // anything else into the Err that becomes the container's non-zero exit.
    #[test]
    fn dispatch_reports_ok_for_a_healthy_server() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let addr = rt.block_on(spawn_server("200 OK"));
        // `dispatch` builds its own runtime, so call it off the async context.
        let url = format!("http://{addr}/health");
        std::thread::spawn(move || dispatch(Some(url), "127.0.0.1", 0, 5))
            .join()
            .expect("thread")
            .expect("a 200 server is healthy");
    }

    #[test]
    fn dispatch_reports_err_for_an_unhealthy_server() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let addr = rt.block_on(spawn_server("503 Service Unavailable"));
        let url = format!("http://{addr}/health");
        let err = std::thread::spawn(move || dispatch(Some(url), "127.0.0.1", 0, 5))
            .join()
            .expect("thread")
            .expect_err("a 503 server is not healthy");
        assert!(
            err.to_string().contains("503"),
            "should name the status: {err}"
        );
    }

    // With no --url, dispatch must derive the target from --host/--port (how MB_PORT reaches it).
    #[test]
    fn dispatch_without_url_probes_the_admin_port() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let addr = rt.block_on(spawn_server("200 OK"));
        let port = addr.port();
        std::thread::spawn(move || dispatch(None, "127.0.0.1", port, 5))
            .join()
            .expect("thread")
            .expect("should have probed the derived admin URL");
    }

    #[test]
    fn default_url_keeps_an_explicit_host() {
        assert_eq!(
            default_url("example.internal", 8080),
            "http://example.internal:8080/health"
        );
    }

    #[test]
    fn default_url_brackets_an_ipv6_literal() {
        assert_eq!(default_url("::1", 2525), "http://[::1]:2525/health");
    }
}
