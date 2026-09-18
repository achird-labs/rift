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
///
/// A `ProbeKey::Sent` key is the raw `Authorization` value — the exact form the admin plane compares
/// (no `Bearer` prefix) — when the server is keyed (issue #1154).
pub async fn probe(url: &str, timeout: Duration, key: ProbeKey<'_>) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("failed to build the healthcheck client")?;

    let mut request = client.get(url);
    if let ProbeKey::Sent(key) = key {
        request = request.header(reqwest::header::AUTHORIZATION, sensitive_header(key)?);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("healthcheck request to {url} failed"))?;

    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // A bare 401 is what made a locked-down container look like a broken one, so say why — and
        // say the right thing: the remedy differs for each of the three states.
        let why = match key {
            ProbeKey::Absent => {
                "the server requires an API key — set `MB_APIKEY` (or rcfile `apiKey`) for the \
                 probe as well"
            }
            ProbeKey::Sent(_) => {
                "the server rejected the configured API key (`MB_APIKEY` / `--api-key` / rcfile \
                 `apiKey`)"
            }
            ProbeKey::Withheld => {
                "an explicit `--url` is never sent the API key; drop `--url` to probe the admin API \
                 with it"
            }
        };
        bail!("healthcheck: {url} answered {status}; {why}");
    }
    if !status.is_success() {
        bail!("healthcheck: {url} answered {status}");
    }

    Ok(())
}

/// What the probe does with the admin API key (issue #1154).
///
/// Three states rather than `Option<&str>`, because "no key is configured" and "a key is configured
/// but deliberately not sent" call for different advice on a `401` — and with an `Option` the probe
/// would tell someone who already set `MB_APIKEY` to go and set `MB_APIKEY`.
#[derive(Debug, Clone, Copy)]
pub enum ProbeKey<'a> {
    /// No key is configured.
    Absent,
    /// A key is configured, and is sent.
    Sent(&'a str),
    /// A key is configured but withheld: the target is an explicit `--url`, which may not be the
    /// admin plane at all.
    Withheld,
}

/// The API key as a header value that reqwest's debug logging will not print.
pub(crate) fn sensitive_header(key: &str) -> Result<reqwest::header::HeaderValue> {
    let mut value = reqwest::header::HeaderValue::from_str(key)
        .context("the configured API key is not a valid header value")?;
    value.set_sensitive(true);
    Ok(value)
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

    format!("http://{}/health", url_authority(host, port))
}

/// `host:port` as a URL authority. A bare IPv6 literal is bracketed, or its port would read as one
/// more hextet; a name, an IPv4 literal and an already-bracketed IPv6 literal pass through.
pub(crate) fn url_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// `rift healthcheck`: probe the admin API. `Err` is the unhealthy verdict — `main` turns it into
/// the non-zero exit Docker reads, and prints the cause for whoever runs `docker inspect`.
///
/// Runs on its own runtime and never touches the server bootstrap — a probe must not write the
/// running server's PID file or log files out from under it.
pub fn dispatch(
    url: Option<String>,
    host: &str,
    port: u16,
    timeout_secs: u64,
    api_key: Option<&str>,
) -> Result<()> {
    // The key goes only to the URL derived from the server's own --host/--port. An explicit --url is
    // an arbitrary operator-chosen target — the in-repo use points it at the *unauthenticated*
    // metrics port — and attaching the admin secret to it would leak the key to wherever that is.
    // An operator who points --url at the admin plane of a keyed server gets the 401 verdict, and
    // the help text says so (issue #1154).
    let (url, key) = match (url, api_key) {
        (Some(explicit), Some(_)) => (explicit, ProbeKey::Withheld),
        (Some(explicit), None) => (explicit, ProbeKey::Absent),
        (None, Some(k)) => (default_url(host, port), ProbeKey::Sent(k)),
        (None, None) => (default_url(host, port), ProbeKey::Absent),
    };
    // One outbound GET; a current-thread runtime is enough (and is what `script_cli` uses).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start the probe runtime")?;

    runtime.block_on(probe(&url, Duration::from_secs(timeout_secs), key))
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

    /// A server that behaves like a keyed admin plane — `401` unless the request carries
    /// `authorization: <key>` exactly (the raw token, no `Bearer`) — and records every request head
    /// it saw, so a test can assert on what the probe actually sent.
    async fn spawn_keyed_server(
        key: &'static str,
    ) -> (
        std::net::SocketAddr,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let authorized = head
                    .lines()
                    .any(|l| l.eq_ignore_ascii_case(&format!("authorization: {key}")));
                log.lock().expect("log").push(head);
                let status = if authorized {
                    "200 OK"
                } else {
                    "401 Unauthorized"
                };
                let resp =
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (addr, seen)
    }

    // Issue #1154: the probe runs in a process that already holds the key (`MB_APIKEY` fills the
    // top-level `api_key` for every subcommand), but it never sent it, so a keyed server looked
    // unhealthy forever.
    #[tokio::test]
    async fn probe_presents_the_api_key_to_a_keyed_server() {
        let (addr, _) = spawn_keyed_server("k").await;
        let url = format!("http://{addr}/health");
        assert!(
            probe(&url, Duration::from_secs(5), ProbeKey::Sent("k"))
                .await
                .is_ok(),
            "a keyed server must answer 200 to a probe that presents the key"
        );
    }

    // Without a key, the 401 must say *why* — a bare status is what made a locked-down container
    // look like a broken server.
    #[tokio::test]
    async fn a_401_without_a_key_names_the_remedy() {
        let (addr, _) = spawn_keyed_server("k").await;
        let url = format!("http://{addr}/health");
        let err = probe(&url, Duration::from_secs(5), ProbeKey::Absent)
            .await
            .expect_err("a keyed server rejects an unkeyed probe");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("MB_APIKEY"),
            "the error must point at the fix, got: {msg}"
        );
    }

    // The key goes only to the URL derived from the server's own --host/--port. An operator-supplied
    // --url is an arbitrary target (the in-repo use points it at the unauthenticated metrics port),
    // and auto-attaching the admin secret to it would leak the key.
    #[test]
    fn an_explicit_url_is_never_sent_the_key() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (addr, seen) = runtime.block_on(spawn_keyed_server("k"));
        // The worker must keep running while dispatch builds its own runtime and probes.
        let _guard = runtime.enter();
        let _ = std::thread::spawn({
            let url = format!("http://{addr}/health");
            move || dispatch(Some(url), "127.0.0.1", 0, 5, Some("k"))
        })
        .join();
        let heads = seen.lock().expect("log");
        assert!(!heads.is_empty(), "the probe must have reached the server");
        assert!(
            heads
                .iter()
                .all(|h| !h.to_ascii_lowercase().contains("authorization")),
            "an explicit --url must never be sent the admin key, got: {heads:?}"
        );
    }

    // The positive sibling of `an_explicit_url_is_never_sent_the_key`, at the same layer. Without it
    // that test passes just as well if `dispatch` dropped the key on *both* branches — so the
    // withholding would be indistinguishable from never sending the key at all.
    #[test]
    fn the_derived_url_is_sent_the_key() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let (addr, _) = runtime.block_on(spawn_keyed_server("k"));
        let _guard = runtime.enter();
        let result =
            std::thread::spawn(move || dispatch(None, "127.0.0.1", addr.port(), 5, Some("k")))
                .join()
                .expect("probe thread");
        assert!(
            result.is_ok(),
            "the derived admin URL must be sent the key and answer 200: {result:?}"
        );
    }

    // A user who set MB_APIKEY but used --url against a keyed admin plane must not be told to set
    // MB_APIKEY — the key was withheld on purpose, and the fix is to drop --url.
    #[tokio::test]
    async fn a_401_on_a_withheld_key_says_to_drop_url_not_to_set_the_key() {
        let (addr, _) = spawn_keyed_server("k").await;
        let url = format!("http://{addr}/health");
        let err = probe(&url, Duration::from_secs(5), ProbeKey::Withheld)
            .await
            .expect_err("a keyed server rejects a probe whose key was withheld");
        let msg = format!("{err:#}");
        assert!(msg.contains("--url"), "must point at --url, got: {msg}");
        assert!(
            !msg.contains("set `MB_APIKEY`"),
            "must not tell someone who already set the key to set it, got: {msg}"
        );
    }

    // A key that was sent but is wrong is a third situation again.
    #[tokio::test]
    async fn a_401_on_a_sent_key_says_the_key_was_rejected() {
        let (addr, _) = spawn_keyed_server("right").await;
        let url = format!("http://{addr}/health");
        let err = probe(&url, Duration::from_secs(5), ProbeKey::Sent("wrong"))
            .await
            .expect_err("a wrong key is rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("rejected"), "got: {msg}");
        assert!(
            !msg.contains("wrong"),
            "the error must never echo the key itself, got: {msg}"
        );
    }

    // The CHANGELOG sells "a debug log never prints it" — so pin the property it rests on.
    #[test]
    fn the_key_header_is_marked_sensitive() {
        let value = sensitive_header("k").expect("a valid header value");
        assert!(value.is_sensitive());
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
        assert!(
            probe(&url, Duration::from_secs(5), ProbeKey::Absent)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn probe_rejects_5xx() {
        let addr = spawn_server("500 Internal Server Error").await;
        let url = format!("http://{addr}/health");
        let err = probe(&url, Duration::from_secs(5), ProbeKey::Absent)
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
        assert!(
            probe(&url, Duration::from_secs(5), ProbeKey::Absent)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn probe_reports_connection_refused() {
        // Bind then drop: the port is now almost certainly free, so connecting is refused.
        let addr = {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("local_addr")
        };
        let url = format!("http://{addr}/health");
        assert!(
            probe(&url, Duration::from_secs(5), ProbeKey::Absent)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn probe_times_out_on_a_hung_server() {
        // Without a timeout the probe would hang here forever and only Docker's own --timeout
        // would end it; the exit code has to come from us.
        let addr = spawn_hung_server().await;
        let url = format!("http://{addr}/health");
        assert!(
            probe(&url, Duration::from_millis(200), ProbeKey::Absent)
                .await
                .is_err()
        );
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
        std::thread::spawn(move || dispatch(Some(url), "127.0.0.1", 0, 5, None))
            .join()
            .expect("thread")
            .expect("a 200 server is healthy");
    }

    #[test]
    fn dispatch_reports_err_for_an_unhealthy_server() {
        let rt = tokio::runtime::Runtime::new().expect("rt");
        let addr = rt.block_on(spawn_server("503 Service Unavailable"));
        let url = format!("http://{addr}/health");
        let err = std::thread::spawn(move || dispatch(Some(url), "127.0.0.1", 0, 5, None))
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
        std::thread::spawn(move || dispatch(None, "127.0.0.1", port, 5, None))
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

    // Issue #1137: the bracketing rule is shared with `rift save`, so it is pinned on its own.
    #[test]
    fn url_authority_brackets_only_a_bare_ipv6_literal() {
        assert_eq!(url_authority("::1", 2525), "[::1]:2525");
        assert_eq!(url_authority("[::1]", 2525), "[::1]:2525");
        assert_eq!(url_authority("127.0.0.1", 2525), "127.0.0.1:2525");
        assert_eq!(url_authority("localhost", 80), "localhost:80");
    }
}
