//! Inbound forward-proxy intercept listener (epic #394, slice 3/5 + slice 4/5).
//!
//! An opt-in listener a SUT points at via `https.proxyHost`/`proxyPort`. It accepts HTTP
//! `CONNECT`, TLS-terminates the tunnel using the per-SNI cert resolver from slice 1
//! ([`SniCertResolver`]), and matches the decrypted request against an [`InterceptRules`] store
//! (slice 4): a matching rule either serves an inline stub or forwards to a named imposter port.
//! With no matching rule (including an empty store), the handler falls back to a fixed 200 that
//! echoes the intercepted host, so slice-3 behavior is unchanged by default.
//!
//! It is entirely opt-in: nothing runs until [`InterceptListener::bind`] is called, so the
//! default imposter-on-a-port model is unchanged.
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::intercept_control::InterceptAuth;
use crate::intercept_rules::{InterceptAction, InterceptRules, ServeStub};
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rift_mock_core::proxy::HttpTuning;
use rift_mock_core::proxy::intercept_ca::SniCertResolver;
use rustls::ServerConfig;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

/// Upper bound on the hand-read `CONNECT` head. Since #991 this bounds *only* that head — the
/// decrypted tunnel's own request head is hyper's to bound, via `HttpTuning::max_buf_size`.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Upper bound on an intercepted request body we will buffer before forwarding/matching. Bounds
/// memory use for a misbehaving or malicious `content-length`.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Per-stage deadline for the CONNECT read, TLS handshake, and request body read. Bounds a slow
/// or silent client so its connection task cannot park indefinitely (slowloris).
///
/// hyper's `header_read_timeout` bounds the request *head* only, so it replaces this deadline for
/// the head and no more — the body read keeps an explicit deadline of its own below.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff after a listener `accept()` error so a persistent failure (e.g. FD exhaustion) does
/// not spin the accept loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);
/// Header slots for parsing the `CONNECT` head. Generous for a `CONNECT`, and overflowing it
/// fails the credential check *closed* — the direction a security classifier must fail in.
const MAX_CONNECT_HEADERS: usize = 128;

/// The `host:port` a client asked to reach via `CONNECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectTarget {
    host: String,
    port: u16,
}

impl std::fmt::Display for ConnectTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Everything a single decrypted tunnel needs to answer requests. Built once per accepted
/// connection and shared by the `service_fn` closure, so a request clones one `Arc` instead of
/// the rule store, the forward client and the host separately.
struct TunnelCtx {
    host: String,
    rules: InterceptRules,
    forward_client: reqwest::Client,
}

/// Why reading an intercepted request body did not produce bytes. The distinctions are the point:
/// only an over-cap body is the client's fault in a way that deserves `413`, answering `413` to a
/// client that simply disconnected mid-body would be a lie, and a body that was framed correctly
/// but arrived too slowly is a timeout rather than a bad request.
#[derive(Debug)]
enum BodyReadError {
    /// The body exceeded [`MAX_BODY_BYTES`].
    TooLarge,
    /// The body did not arrive within [`IO_TIMEOUT`]. The framing was fine, so this is `408`, not
    /// a claim that the client sent something malformed.
    Timeout,
    /// The body could not be read to completion (client went away, framing error).
    Incomplete(String),
}

/// A running intercept listener. Call [`InterceptListener::shutdown`] for a clean stop — that is
/// the only way to *wait* for the accept loop to exit.
///
/// The stop signal is a broadcast rather than a watch (issue #1010) because it has two audiences:
/// the accept loop, and every decrypted tunnel currently being served. Holding the only `Sender`
/// here is what makes dropping the listener safe as well — every receiver observes the close and
/// drains, instead of tunnels outliving the handle that owned them.
pub struct InterceptListener {
    local_addr: SocketAddr,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    handle: JoinHandle<()>,
}

impl InterceptListener {
    /// Bind an intercept listener on `addr` and start accepting connections. Use `127.0.0.1:0`
    /// to get an OS-assigned port (read it back via [`local_addr`](Self::local_addr)).
    ///
    /// `rules` is matched against every intercepted request (issue #398); an empty store falls
    /// back to the fixed slice-3 200 response.
    ///
    /// `auth`, when set, requires `Proxy-Authorization: Basic …` on every `CONNECT` (issue #878);
    /// `None` leaves the proxy open, which is the behaviour it has always had.
    pub async fn bind(
        addr: SocketAddr,
        resolver: Arc<SniCertResolver>,
        rules: InterceptRules,
        auth: Option<InterceptAuth>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;
        let tls = build_tls_acceptor(resolver)?;

        // One forward client per listener, cloned per connection (issue #552). Nothing about it
        // varies per request — the target port lives in the URL, and reqwest pools per host:port —
        // so building it per request only discarded the connection pool. Owned by the listener
        // rather than a process-wide static so the pool dies with `stop()` and embedded
        // multi-instance use keeps pools independent; building here also surfaces a failure as a
        // start error instead of a lazy-init panic.
        let forward_client = build_forward_client()?;
        let auth = auth.map(Arc::new);
        // Read once at bind rather than per connection: the knobs are process-wide env vars, and
        // this listener now shares them with every other one (issue #991).
        let http_tuning = HttpTuning::from_env();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            let tls = tls.clone();
                            let rules = rules.clone();
                            let forward_client = forward_client.clone();
                            let auth = auth.clone();
                            // Taken here, before the spawn: a broadcast receiver only sees sends
                            // that happen after it exists, so subscribing inside the task would
                            // race `shutdown()` and could miss the signal entirely. Derived from
                            // the loop's own receiver rather than from a `Sender` clone — holding
                            // a second sender in here would stop `recv()` ever reporting `Closed`,
                            // and the drop path above depends on it doing so.
                            let conn_shutdown_rx = shutdown_rx.resubscribe();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, tls, rules, forward_client, auth, http_tuning, conn_shutdown_rx).await {
                                    tracing::debug!(%peer, error = %e, "intercept connection ended");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "intercept listener accept failed");
                            tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        }
                    },
                }
            }
        });

        Ok(Self {
            local_addr,
            shutdown_tx,
            handle,
        })
    }

    /// The address the listener is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signal the accept loop to stop and wait for it to finish.
    ///
    /// The same signal reaches every tunnel in flight, which stops accepting new requests on its
    /// connection and closes once the current one completes (issue #1010). Like the imposter path
    /// it mirrors, this does not *await* those connections — an in-flight request finishes in the
    /// background; an idle tunnel closes at once.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        log_accept_loop_exit(self.handle.await);
    }
}

/// Surface an abnormal exit of the accept-loop task (issue #522). The loop returns `()` and is
/// never aborted, so a `JoinError` here means it panicked — log it instead of silently discarding
/// the join result, which would let `shutdown`/`stop` report success over a crashed listener.
fn log_accept_loop_exit(result: Result<(), tokio::task::JoinError>) {
    if let Err(e) = result {
        tracing::warn!(error = %e, "intercept listener accept loop ended abnormally");
    }
}

/// The client used for every `InterceptAction::Forward`. Relays the imposter's own response
/// verbatim: never follow redirects (a 3xx from the imposter is a response to hand back, not to
/// chase). `.timeout` is reqwest's per-request total-time bound, so sharing one client across
/// requests keeps each forward bounded by `IO_TIMEOUT` individually — it is not a client lifetime.
fn build_forward_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(IO_TIMEOUT)
        .build()
        .map_err(|e| anyhow::anyhow!("building forward client: {e}"))
}

fn build_tls_acceptor(resolver: Arc<SniCertResolver>) -> anyhow::Result<TlsAcceptor> {
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow::anyhow!("intercept TLS config: {e}"))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
    // Only HTTP/1.1 for now (non-goal: h2/websocket, see #394).
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // Explicit TLS session resumption (issue #705): the intercept listener sees the same
    // handshake-storm reconnect pattern as the imposters, so it shares their resumption config.
    rift_mock_core::proxy::configure_session_resumption(&mut config)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn handle_connection(
    mut stream: TcpStream,
    tls: TlsAcceptor,
    rules: InterceptRules,
    forward_client: reqwest::Client,
    auth: Option<Arc<InterceptAuth>>,
    http_tuning: HttpTuning,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let head = timeout(IO_TIMEOUT, read_connect_head(&mut stream))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading CONNECT head"))??;
    let Some(target) = parse_connect(&head) else {
        stream
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nconnection: close\r\n\r\n")
            .await?;
        return Ok(());
    };

    // Issue #878, and the ordering is load-bearing: this sits BEFORE the `200` below, because TLS
    // only starts after it. Checking here means an unauthenticated caller never reaches the
    // handshake, so Rift never mints a leaf certificate for an SNI of their choosing. Moving this
    // after the `200` would still refuse the request while doing exactly the work worth refusing.
    if let Some(ref auth) = auth
        && !proxy_credential_matches(&head, auth)
    {
        stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                  Proxy-Authenticate: Basic realm=\"rift-intercept\"\r\n\
                  connection: close\r\n\r\n",
            )
            .await?;
        tracing::debug!(%target, "intercept CONNECT refused: missing or invalid Proxy-Authorization");
        return Ok(());
    }

    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;

    let tls_stream = match timeout(IO_TIMEOUT, tls.accept(stream)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // A broken cert resolver (e.g. misconfigured intercept CA) fails EVERY handshake, so
            // log at warn — distinct from a client that simply closed early.
            tracing::warn!(%target, error = %e, "intercept TLS handshake failed");
            return Ok(());
        }
        Err(_) => {
            tracing::warn!(%target, "intercept TLS handshake timed out");
            return Ok(());
        }
    };

    let ctx = Arc::new(TunnelCtx {
        host: target.host,
        rules,
        forward_client,
    });
    serve_tunnel(TokioIo::new(tls_stream), ctx, http_tuning, shutdown_rx).await;
    Ok(())
}

/// Serve the decrypted tunnel with hyper (issue #991), following how every imposter connection is
/// served (`rift_mock_core::imposter::manager::run_http1`). Request framing, header validation and
/// response serialization are hyper's job from here down.
///
/// Including the shutdown handling: like `run_http1`, this selects on a shutdown receiver and calls
/// `graceful_shutdown()` (issue #1010), so [`InterceptListener::shutdown`] drains in-flight tunnels
/// instead of abandoning them.
///
/// A connection still in its TLS handshake has not reached this function and so does not observe
/// the signal — the same as the imposter path, and bounded by the handshake itself.
async fn serve_tunnel<I>(
    io: I,
    ctx: Arc<TunnelCtx>,
    http_tuning: HttpTuning,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin,
{
    let service = service_fn(move |req: Request<Incoming>| {
        let ctx = Arc::clone(&ctx);
        // Infallible: every failure below becomes a *response*, so a bad request never collapses
        // into a bare connection reset the client cannot interpret.
        async move { Ok::<_, std::convert::Infallible>(handle_tunnel_request(req, ctx).await) }
    });

    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        // A timer is required for `header_read_timeout` to take effect (hyper panics on
        // serve_connection otherwise) — always paired with it.
        .timer(TokioTimer::new())
        .header_read_timeout(http_tuning.header_read_timeout)
        .max_buf_size(http_tuning.max_buf_size)
        // Keep-alive across the tunnel is issue #993 and deliberately NOT part of this change.
        // hyper defaults to keep-alive on HTTP/1.1, so leaving this out would ship #993 by
        // accident — and #995 records why that ordering is dangerous rather than merely early.
        .keep_alive(false);

    let conn = builder.serve_connection(io, service);
    tokio::pin!(conn);
    tokio::select! {
        res = conn.as_mut() => {
            if let Err(e) = res {
                tracing::debug!(error = %e, "intercept tunnel connection ended");
            }
        }
        // Any `recv()` outcome means stop: `Ok` is an explicit shutdown, `Closed` is the listener
        // being dropped without one. Matching only `Ok` would leave a dropped listener's tunnels
        // running, which is the case the broadcast sender's ownership was arranged to cover.
        _ = shutdown_rx.recv() => {
            conn.as_mut().graceful_shutdown();
            if let Err(e) = conn.as_mut().await {
                tracing::debug!(error = %e, "intercept tunnel connection ended during shutdown");
            }
        }
    }
}

/// Answer one intercepted request. Infallible by construction: a forward failure, an over-cap
/// body and an unreadable body each map to a status rather than to a dropped connection.
async fn handle_tunnel_request(
    req: Request<Incoming>,
    ctx: Arc<TunnelCtx>,
) -> Response<Full<Bytes>> {
    let (parts, incoming) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    // A `content-length` of any value — `0` included — means the client framed a body, which is
    // what the hand-rolled reader keyed on. Preserving that distinction keeps `Some("")` and
    // `None` telling the same two stories to body predicates as before.
    let framed_body = parts.headers.contains_key(CONTENT_LENGTH);
    let headers = collect_request_headers(&parts.headers);

    // The cap is enforced by reading through `Limited`, deliberately NOT by pre-checking the
    // declared `content-length`. Refusing before the read looks cheaper, but it closes the socket
    // while the client is still sending, so the client gets a broken pipe instead of the status —
    // measured: a body one byte over the cap never sees its own 413. Reading to the cap first
    // costs a bounded 1 MiB and is what makes the refusal deliverable. It also needs no trust in
    // a header a malicious client controls.
    let body_bytes = match read_limited_body(incoming).await {
        Ok(bytes) if framed_body || !bytes.is_empty() => Some(bytes),
        Ok(_) => None,
        Err(BodyReadError::TooLarge) => {
            tracing::warn!(
                host = %ctx.host,
                cap = MAX_BODY_BYTES,
                "intercepted request body exceeds cap; refusing with 413"
            );
            return status_response(StatusCode::PAYLOAD_TOO_LARGE);
        }
        Err(BodyReadError::Timeout) => {
            tracing::debug!(
                host = %ctx.host,
                timeout = ?IO_TIMEOUT,
                "intercepted request body did not arrive in time"
            );
            return status_response(StatusCode::REQUEST_TIMEOUT);
        }
        Err(BodyReadError::Incomplete(reason)) => {
            tracing::debug!(host = %ctx.host, reason, "intercepted request body could not be read");
            return status_response(StatusCode::BAD_REQUEST);
        }
    };
    let body = body_bytes.as_deref().map(classify_intercept_body);

    let action = ctx.rules.match_request(
        &ctx.host,
        &method,
        &path,
        query.as_deref(),
        &headers,
        body.as_deref(),
    );

    match action {
        Some(InterceptAction::Serve(stub)) => stub_response(&stub),
        Some(InterceptAction::Forward(forward)) => {
            match forward_response(
                &method,
                &path,
                query.as_deref(),
                &headers,
                body_bytes.as_deref(),
                forward.port,
                &ctx.forward_client,
            )
            .await
            {
                Ok(response) => response,
                Err(e) => {
                    tracing::warn!(
                        host = %ctx.host,
                        port = forward.port,
                        error = %format_args!("{e:#}"),
                        "intercept forward failed"
                    );
                    status_response(StatusCode::BAD_GATEWAY)
                }
            }
        }
        None => no_rule_response(&method, &path, &ctx.host),
    }
}

/// The fixed `200` an unconfigured host falls through to (slice 3), so a SUT pointed at the proxy
/// with no rules gets an answer rather than a hang.
fn no_rule_response(method: &str, path: &str, host: &str) -> Response<Full<Bytes>> {
    let body = Bytes::from(format!("rift intercepted {method} {path} for {host}\n"));
    let len = body.len();
    let mut response = Response::new(Full::new(body));
    let out = response.headers_mut();
    out.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    out.insert(CONTENT_LENGTH, HeaderValue::from(len));
    response
}

/// Buffer the request body, capped at [`MAX_BODY_BYTES`] by `http_body_util::Limited` rather than
/// by a truncating read — a truncated body forwarded as if complete is issue #995.
///
/// The explicit deadline is not redundant with hyper's `header_read_timeout`: that bounds the
/// request head only, so without this a client could send its headers promptly and then dribble a
/// body forever (AC4).
async fn read_limited_body(incoming: Incoming) -> Result<Bytes, BodyReadError> {
    match timeout(IO_TIMEOUT, Limited::new(incoming, MAX_BODY_BYTES).collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(e)) if is_length_limit_error(e.as_ref()) => Err(BodyReadError::TooLarge),
        Ok(Err(e)) => Err(BodyReadError::Incomplete(e.to_string())),
        Err(_) => Err(BodyReadError::Timeout),
    }
}

/// Is this the cap being hit, rather than the body simply failing to arrive? Today `Limited` boxes
/// `LengthLimitError` as the outermost error, but the whole source chain is walked so that a future
/// hyper or `http-body-util` that wraps it cannot silently downgrade every `413` into a `400`.
fn is_length_limit_error(err: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(err), |e| e.source())
        .any(|e| e.is::<http_body_util::LengthLimitError>())
}

/// Flatten hyper's request headers into the `name -> value` map the rule matcher takes.
///
/// Two deliberate carry-overs: repeated headers still collapse to the last value (widening that is
/// issue #994), and a value that is not UTF-8 is dropped rather than passed through
/// `from_utf8_lossy` — predicates used to be evaluated against U+FFFD garbage the client never sent.
fn collect_request_headers(headers: &hyper::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(headers.len());
    for (name, value) in headers {
        // `HeaderName` is always lowercase, so matcher lookups stay case-insensitive for free.
        match value.to_str() {
            Ok(text) => {
                out.insert(name.as_str().to_string(), text.to_string());
            }
            Err(_) => {
                tracing::debug!(header = %name, "skipping non-UTF-8 intercepted request header value")
            }
        }
    }
    out
}

/// A bodyless response carrying only a status — the shape `502` has always had, reused for the
/// other refusals so none of them invent a body a client would have to parse.
fn status_response(status: StatusCode) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

/// Render an [`InterceptAction::Serve`] stub as a response. `content-length` is always computed
/// here and `connection` is hyper's to set, so both override any same-named entry in
/// `stub.headers` and the response stays well-formed regardless of stub configuration.
///
/// Header names and values are built through `HeaderName`/`HeaderValue`, which reject CR/LF by
/// construction — the response-splitting guard #936 had to hand-write is now unrepresentable
/// rather than merely relocated.
fn stub_response(stub: &ServeStub) -> Response<Full<Bytes>> {
    let body = Bytes::copy_from_slice(stub.body_str().as_bytes());
    let status = StatusCode::from_u16(stub.status_code).unwrap_or_else(|_| {
        tracing::warn!(
            status = stub.status_code,
            "intercept stub status is not a valid HTTP status; serving 500"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    });

    let len = body.len();
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    let out = response.headers_mut();
    for (name, values) in &stub.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        // A name that is not a legal header name disqualifies every one of its values; a value
        // that is not legal takes only itself out, so one bad value never drops its clean
        // siblings (the per-value granularity #936 established).
        let Ok(header_name) = HeaderName::try_from(name.as_str()) else {
            tracing::warn!(header = %name, "skipping invalid intercept stub header name");
            continue;
        };
        for value in values {
            match HeaderValue::try_from(value.as_str()) {
                // One entry per value (issue #936); comma-joining would be wrong for `set-cookie`,
                // which is the header multi-value support exists for.
                Ok(header_value) => {
                    out.append(header_name.clone(), header_value);
                }
                Err(_) => {
                    tracing::warn!(header = %name, "skipping invalid intercept stub header value")
                }
            }
        }
    }
    out.insert(CONTENT_LENGTH, HeaderValue::from(len));
    response
}

/// Forward the decrypted request to `http://127.0.0.1:{port}{path}[?query]` and return the
/// upstream status, headers, and body as the tunnel's response. Returns `Err` on any connection or
/// I/O failure so the caller can answer `502 Bad Gateway` without panicking.
async fn forward_response(
    method: &str,
    path: &str,
    query: Option<&str>,
    headers: &HashMap<String, String>,
    body: Option<&[u8]>,
    port: u16,
    client: &reqwest::Client,
) -> anyhow::Result<Response<Full<Bytes>>> {
    let url = match query {
        Some(q) => format!("http://127.0.0.1:{port}{path}?{q}"),
        None => format!("http://127.0.0.1:{port}{path}"),
    };
    let reqwest_method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|e| anyhow::anyhow!("invalid method '{method}': {e}"))?;

    let mut builder = client.request(reqwest_method, &url);
    for (name, value) in headers {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(bytes) = body {
        builder = builder.body(bytes.to_vec());
    }

    let upstream = builder
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("forward to 127.0.0.1:{port} failed: {e}"))?;

    let status = StatusCode::from_u16(upstream.status().as_u16()).map_err(|e| {
        anyhow::anyhow!("upstream 127.0.0.1:{port} returned an invalid status: {e}")
    })?;
    let upstream_headers = upstream.headers().clone();
    let body_bytes = upstream
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("reading upstream body from 127.0.0.1:{port}: {e}"))?;

    let len = body_bytes.len();
    let mut response = Response::new(Full::new(body_bytes));
    *response.status_mut() = status;
    let out = response.headers_mut();
    for (name, value) in upstream_headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        // reqwest and hyper share one `http` crate in this workspace, so these are the same types
        // and both clones are refcount bumps. Cloning rather than rebuilding from bytes also means
        // there is no conversion that can fail, so no upstream header can be silently dropped on
        // its way back to the client — including a legal non-UTF-8 value, which the pre-#991 code
        // dropped because it relayed through `to_str()`.
        out.append(name.clone(), value.clone());
    }
    // `content-length` is hop-by-hop above, so the upstream's is always dropped and the real
    // length of what we buffered is always what goes out.
    out.insert(CONTENT_LENGTH, HeaderValue::from(len));
    Ok(response)
}

/// Hop-by-hop / connection-management headers we recompute ourselves rather than pass through
/// verbatim in either direction (request forwarding or response relaying).
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "connection" | "content-length" | "transfer-encoding"
    )
}

/// Read the `CONNECT` request head one byte at a time, stopping exactly at the terminating
/// `\r\n\r\n`. Reading byte-by-byte avoids consuming any TLS ClientHello bytes that follow — the
/// client sends those only after our `200` response, but a buffered read could still over-read.
async fn read_connect_head(stream: &mut TcpStream) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            anyhow::bail!("connection closed before CONNECT head completed");
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEAD_BYTES {
            anyhow::bail!("CONNECT head exceeds {MAX_HEAD_BYTES} bytes");
        }
    }
    Ok(buf)
}

/// Look one header up in the hand-read `CONNECT` head.
///
/// The tunnel's own requests are parsed by hyper, but the `CONNECT` head is read by hand on
/// purpose (see [`read_connect_head`]) and so still needs parsing — `httparse` is hyper's own
/// head parser, so this is the same code doing it, not a second hand-rolled one.
fn connect_header(head: &[u8], name: &str) -> Option<String> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_CONNECT_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    // Every error path here — a malformed head, more than MAX_CONNECT_HEADERS headers, a
    // non-UTF-8 value — yields `None`, i.e. "no such header", which is what fails the credential
    // check closed.
    request.parse(head).ok()?;
    // Last occurrence wins, which is what the `HashMap` this replaced did. Pinned as intentional
    // by `a_duplicate_header_takes_the_last_value` (#878): Rift is the terminal parser here, so a
    // duplicate cannot be smuggled past a downstream hop, but the behaviour should change
    // deliberately rather than as a side effect of swapping the parser.
    request
        .headers
        .iter()
        .rev()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(h.value).ok())
        .map(|value| value.trim().to_string())
}

/// Does the `CONNECT` head carry a `Proxy-Authorization: Basic …` matching `expected` (issue #878)?
///
/// Fails **closed** at every step it cannot complete: a missing header, a non-Basic scheme,
/// undecodable base64, non-UTF-8 bytes or a value with no `:` all return `false`. A classifier that
/// cannot read the credential must treat it as absent, never as valid.
fn proxy_credential_matches(head: &[u8], expected: &InterceptAuth) -> bool {
    let Some(value) = connect_header(head, "proxy-authorization") else {
        return false;
    };
    let Some(encoded) = value
        .split_once(char::is_whitespace)
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
        .map(|(_, rest)| rest.trim())
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return false;
    };

    // Compare the whole `user:pass` in one pass, so timing cannot reveal whether the username alone
    // was right — which comparing the halves separately would leak. `ct_eq` short-circuits on a
    // length mismatch, so the credential's *length* is still observable; that is inherent to
    // comparing variable-length secrets and is the same bound `api_key_matches` accepts.
    let expected_pair = format!("{}:{}", expected.username, expected.password);
    expected_pair.as_bytes().ct_eq(&decoded).into()
}

fn parse_connect(head: &[u8]) -> Option<ConnectTarget> {
    let text = std::str::from_utf8(head).ok()?;
    let line = text.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let authority = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("CONNECT") || authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        // Reject a malformed port rather than silently defaulting — it signals a broken client.
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, 443),
    };
    if host.is_empty() {
        return None;
    }
    Some(ConnectTarget {
        host: host.to_string(),
        port,
    })
}

/// Classify an intercepted request body for rule matching (issue #646): valid UTF-8 is matched
/// as-is (borrowed, no copy); a binary body (protobuf, gzip, an image) is matched against its
/// standard base64 encoding — the same convention as recorded requests (#636) and binary
/// responses (#117). `from_utf8_lossy` used to replace every invalid byte with U+FFFD, so body
/// predicates evaluated against garbage the client never sent. Forwarding is unaffected: it
/// sends the raw `body_bytes`.
fn classify_intercept_body(bytes: &[u8]) -> Cow<'_, str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(base64::engine::general_purpose::STANDARD.encode(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intercept_rules::{ForwardTarget, InterceptRule};
    use rift_mock_core::proxy::intercept_ca::CertificateAuthority;

    /// A port with nothing listening, for the tests that need a forward target that always fails.
    /// Port 1 is below the privileged threshold, so no test can bind it, and outside the ephemeral
    /// range `bind(:0)` allocates from, so the allocator can never hand it out — `connect` here is
    /// always refused. These tests used to bind an ephemeral port and drop the listener, which
    /// returns the number to the allocator immediately; under full-suite parallelism another test
    /// takes it and the "dead" upstream answers (issue #859).
    const CLOSED_PORT: u16 = 1;

    // ===== Issue #991: the stub is rendered as a `hyper::Response`, not hand-serialized =====

    /// The stub's headers and body, read back off the `Response` `stub_response` builds. Reading
    /// the typed response rather than a rendered byte string is the point: the assertions below
    /// are about what hyper is handed, and hyper owns the serialization from there.
    async fn rendered_stub(stub: &ServeStub) -> (StatusCode, hyper::HeaderMap, String) {
        let response = stub_response(stub);
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("a Full<Bytes> body cannot fail to collect")
            .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("the test stubs are all valid UTF-8");
        (status, headers, body)
    }

    fn header_values<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Vec<&'a str> {
        headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().expect("test headers are ASCII"))
            .collect()
    }

    // Issue #936 / AC3: a header with several values becomes several header entries. Collapsing
    // them into one comma-joined value is wrong for `set-cookie`, which is the header that
    // motivates multi-value support at all.
    #[tokio::test]
    async fn stub_response_emits_one_entry_per_header_value() {
        let stub = ServeStub::new(
            200,
            HashMap::from([
                (
                    "set-cookie".to_string(),
                    vec!["a=1".to_string(), "b=2".to_string()],
                ),
                (
                    "content-type".to_string(),
                    vec!["application/json".to_string()],
                ),
            ]),
            None,
        );
        let (status, headers, _) = rendered_stub(&stub).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            header_values(&headers, "set-cookie"),
            vec!["a=1", "b=2"],
            "two values, two entries — never comma-joined"
        );
        assert_eq!(
            header_values(&headers, "content-type"),
            vec!["application/json"]
        );
    }

    // Issue #936 / AC2: the CR/LF response-splitting guard has to apply per value, not per header
    // name — otherwise widening to multi-value would quietly reopen the injection hole it closed.
    // The guard is no longer hand-written: `HeaderValue` refuses to hold CR/LF at all.
    #[tokio::test]
    async fn stub_response_skips_only_the_injecting_header_value() {
        let stub = ServeStub::new(
            200,
            HashMap::from([(
                "x-multi".to_string(),
                vec![
                    "clean".to_string(),
                    "bad\r\nx-injected: yes".to_string(),
                    "also-clean".to_string(),
                ],
            )]),
            None,
        );
        let (_, headers, _) = rendered_stub(&stub).await;

        assert!(
            headers.get("x-injected").is_none(),
            "the CR/LF-bearing value is dropped, so it cannot smuggle a header"
        );
        assert_eq!(
            header_values(&headers, "x-multi"),
            vec!["clean", "also-clean"],
            "its clean siblings still go out"
        );
    }

    // AC2's other half: a poisoned header *name* disqualifies every one of its values, since a
    // name is not something a single value can be dropped from.
    #[tokio::test]
    async fn stub_response_skips_a_header_whose_name_is_poisoned() {
        let stub = ServeStub::new(
            200,
            HashMap::from([(
                "x-bad\r\nx-injected".to_string(),
                vec!["whatever".to_string()],
            )]),
            None,
        );
        let (_, headers, _) = rendered_stub(&stub).await;
        assert!(headers.get("x-injected").is_none());
        assert!(
            !headers.iter().any(|(n, _)| n.as_str().contains("x-bad")),
            "the whole header is dropped, not just its value"
        );
    }

    // Issue #933: the object body is serialized compactly, and `content-length` counts the
    // rendered bytes.
    #[tokio::test]
    async fn stub_response_serves_object_body_as_compact_json() {
        let stub = ServeStub::new(
            200,
            HashMap::from([(
                "content-type".to_string(),
                vec!["application/json".to_string()],
            )]),
            Some(serde_json::json!({ "featureX": "ON" })),
        );
        let (status, headers, body) = rendered_stub(&stub).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"featureX":"ON"}"#);
        assert_eq!(header_values(&headers, "content-length"), vec!["17"]);
    }

    // `content-length` is a *byte* count — a multi-byte body must not report its character count.
    #[tokio::test]
    async fn stub_response_object_body_content_length_is_bytes() {
        let stub = ServeStub::new(
            200,
            HashMap::new(),
            Some(serde_json::json!({ "msg": "héllo→" })),
        );
        let (_, headers, body) = rendered_stub(&stub).await;
        assert!(
            body.chars().count() < body.len(),
            "the body is multi-byte, so a character count would differ from the byte count"
        );
        assert_eq!(
            header_values(&headers, "content-length"),
            vec![body.len().to_string().as_str()],
            "content-length is the byte length, not the character count"
        );
    }

    // AC3: the pre-#933 behaviour for string and absent bodies is unchanged.
    #[tokio::test]
    async fn stub_response_string_and_absent_bodies_are_unchanged() {
        let string_body = ServeStub::new(
            200,
            HashMap::new(),
            Some(serde_json::json!(r#"{"featureX":"ON"}"#)),
        );
        let (_, _, body) = rendered_stub(&string_body).await;
        assert_eq!(
            body, r#"{"featureX":"ON"}"#,
            "a string body reaches the wire byte-identically to before the widening"
        );

        let (status, headers, body) =
            rendered_stub(&ServeStub::new(404, HashMap::new(), None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(header_values(&headers, "content-length"), vec!["0"]);
        assert_eq!(body, "", "no body follows the head");
    }

    // The framing headers are ours, not the stub's: a stub that sets `content-length` cannot make
    // the response disagree with its own body, and one that sets `connection` cannot re-open the
    // keep-alive question issue #993 owns.
    #[tokio::test]
    async fn stub_response_overrides_stub_supplied_framing_headers() {
        let stub = ServeStub::new(
            200,
            HashMap::from([
                ("content-length".to_string(), vec!["99999".to_string()]),
                ("connection".to_string(), vec!["keep-alive".to_string()]),
                ("host".to_string(), vec!["elsewhere.example".to_string()]),
                ("transfer-encoding".to_string(), vec!["chunked".to_string()]),
            ]),
            Some(serde_json::json!("hi")),
        );
        let (_, headers, body) = rendered_stub(&stub).await;
        assert_eq!(body, "hi");
        assert_eq!(
            header_values(&headers, "content-length"),
            vec!["2"],
            "the real body length wins over the stub's claim"
        );
        for hop_by_hop in ["connection", "host", "transfer-encoding"] {
            assert!(
                headers.get(hop_by_hop).is_none(),
                "{hop_by_hop} is ours to manage, not the stub's"
            );
        }
    }

    // ===== Issue #991: an over-cap body is told apart from a body that never arrived =====
    //
    // The whole reason `BodyReadError` has two variants: mapping every body failure to `413` would
    // tell a client that disconnected mid-upload that its (unsent) body was too large. The error
    // is produced by driving `Limited` for real rather than synthesised — `LengthLimitError` is
    // `#[non_exhaustive]`, and a hand-built stand-in would not prove the shape hyper actually
    // hands us (it arrives wrapped, so matching only the outermost error would miss it).
    #[tokio::test]
    async fn only_the_cap_being_hit_reads_as_too_large() {
        let over_cap = Limited::new(Full::new(Bytes::from_static(b"0123456789")), 4)
            .collect()
            .await
            .expect_err("ten bytes under a four-byte cap must fail");
        assert!(
            is_length_limit_error(over_cap.as_ref()),
            "the cap being hit is what 413 is for, however deeply it is wrapped"
        );

        let disconnected: Box<dyn std::error::Error + Send + Sync> = Box::new(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection reset",
        ));
        assert!(
            !is_length_limit_error(disconnected.as_ref()),
            "a mid-body disconnect must not be reported as an oversize body"
        );
    }

    // ===== Issue #991: request headers reach the matcher with the same shape as before =====
    #[test]
    fn collect_request_headers_keeps_last_wins_and_drops_non_utf8() {
        let mut headers = hyper::HeaderMap::new();
        headers.append("x-repeat", HeaderValue::from_static("first"));
        headers.append("x-repeat", HeaderValue::from_static("second"));
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));
        headers.insert(
            "x-binary",
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("a legal but non-UTF-8 header value"),
        );

        let collected = collect_request_headers(&headers);

        assert_eq!(
            collected.get("x-repeat").map(String::as_str),
            Some("second"),
            "repeated headers still collapse to the last value; widening that is issue #994"
        );
        assert_eq!(
            collected.get("content-type").map(String::as_str),
            Some("application/json"),
            "lookups stay case-insensitive because hyper's header names are already lowercase"
        );
        assert!(
            !collected.contains_key("x-binary"),
            "a non-UTF-8 value is dropped, not mangled into U+FFFD for predicates to match"
        );
    }

    // Issue #522: a panicked accept loop must not be swallowed by `shutdown`/`stop` — its
    // `JoinError` is logged rather than discarded.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn log_accept_loop_exit_warns_on_panic() {
        // A genuine `JoinError` from a panicked task (its only real source here).
        let joined = tokio::spawn(async { panic!("accept loop boom") }).await;
        assert!(joined.is_err(), "a panicked task yields a JoinError");
        log_accept_loop_exit(joined);
        assert!(
            logs_contain("intercept listener accept loop ended abnormally"),
            "an abnormal accept-loop exit is warned, not swallowed"
        );
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn log_accept_loop_exit_silent_on_normal_exit() {
        log_accept_loop_exit(Ok(()));
        assert!(
            !logs_contain("accept loop ended abnormally"),
            "a clean shutdown logs nothing"
        );
    }

    // Issue #646: a binary intercepted body must reach rule matching as lossless base64, not
    // `from_utf8_lossy`'s U+FFFD-mangled garbage.
    #[test]
    fn classify_intercept_body_base64_round_trips_invalid_utf8() {
        let original: &[u8] = &[0xFF, 0xFE, 0x00, 0x01, 0x02];
        let classified = classify_intercept_body(original);
        assert!(
            !classified.contains('\u{FFFD}'),
            "no replacement-character corruption"
        );
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(classified.as_ref())
            .expect("valid standard base64");
        assert_eq!(decoded, original, "classification is lossless");
    }

    #[test]
    fn classify_intercept_body_text_passthrough() {
        let classified = classify_intercept_body(b"hello world");
        assert_eq!(
            classified.as_ref(),
            "hello world",
            "valid UTF-8 matches as-is; text traffic behaviour is unchanged"
        );
        assert!(
            matches!(classified, Cow::Borrowed(_)),
            "text bodies must not pay an allocation (issue #561 convention)"
        );
        assert_eq!(
            classify_intercept_body(b"").as_ref(),
            "",
            "an empty body classifies as empty text, matching the pre-fix behaviour"
        );
    }

    // Issue #646 end-to-end semantic: a body predicate written against the base64 string matches
    // a binary body, and a predicate written against the old lossy garbage no longer does.
    #[test]
    fn binary_body_predicate_matches_base64_not_lossy_garbage() {
        let binary: &[u8] = &[0x1F, 0x8B, 0x08, 0x00, 0xFF, 0xFE];
        let b64 = base64::engine::general_purpose::STANDARD.encode(binary);
        let lossy = String::from_utf8_lossy(binary).into_owned();
        assert_ne!(b64, lossy, "the two conventions must be distinguishable");

        let body_equals = |needle: &str| -> rift_types::Predicate {
            serde_json::from_value(serde_json::json!({ "equals": { "body": needle } }))
                .expect("valid predicate JSON")
        };
        let serve = |marker: &str| {
            InterceptAction::Serve(ServeStub::new(
                200,
                HashMap::new(),
                Some(serde_json::Value::String(marker.to_string())),
            ))
        };

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![body_equals(&lossy)],
                action: serve("lossy"),
            })
            .unwrap();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![body_equals(&b64)],
                action: serve("base64"),
            })
            .unwrap();

        let classified = classify_intercept_body(binary);
        let action = rules.match_request(
            "cdn.example.com",
            "POST",
            "/upload",
            None,
            &HashMap::new(),
            Some(classified.as_ref()),
        );
        match action {
            Some(InterceptAction::Serve(stub)) => assert_eq!(
                stub.body_str(),
                "base64",
                "the base64-keyed rule matches; the lossy-keyed rule does not"
            ),
            other => panic!("expected the base64-keyed serve rule to match, got {other:?}"),
        }
    }

    // Issue #878: `proxy_credential_matches` is a security classifier, so its fail-closed branches
    // are the point of it. The integration tests cover missing / wrong / non-Basic over a real
    // socket; these cover the decode paths that are awkward to provoke there and would otherwise
    // rest entirely on the doc comment's word.
    mod proxy_auth {
        use super::*;

        fn auth() -> InterceptAuth {
            InterceptAuth {
                username: "ci".to_string(),
                password: "s3cr3t".to_string(),
            }
        }

        fn head(extra: &str) -> Vec<u8> {
            format!("CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: cdn.example.com\r\n{extra}\r\n")
                .into_bytes()
        }

        fn encoded(raw: &str) -> String {
            base64::engine::general_purpose::STANDARD.encode(raw)
        }

        #[test]
        fn accepts_only_the_exact_credential() {
            let ok = head(&format!(
                "Proxy-Authorization: Basic {}\r\n",
                encoded("ci:s3cr3t")
            ));
            assert!(proxy_credential_matches(&ok, &auth()));
        }

        #[test]
        fn fails_closed_on_every_unreadable_credential() {
            let cases = vec![
                ("no header at all", head("")),
                (
                    "wrong password",
                    head(&format!(
                        "Proxy-Authorization: Basic {}\r\n",
                        encoded("ci:wrong")
                    )),
                ),
                (
                    "right password, wrong user",
                    head(&format!(
                        "Proxy-Authorization: Basic {}\r\n",
                        encoded("nope:s3cr3t")
                    )),
                ),
                (
                    "non-Basic scheme",
                    head("Proxy-Authorization: Bearer abc\r\n"),
                ),
                (
                    "scheme with no value",
                    head("Proxy-Authorization: Basic\r\n"),
                ),
                (
                    "undecodable base64",
                    head("Proxy-Authorization: Basic !!!not-base64!!!\r\n"),
                ),
                (
                    "decodes, but carries no colon",
                    head(&format!(
                        "Proxy-Authorization: Basic {}\r\n",
                        encoded("cis3cr3t")
                    )),
                ),
                (
                    "empty credential",
                    head(&format!("Proxy-Authorization: Basic {}\r\n", encoded(""))),
                ),
                (
                    // The `Authorization` header is NOT a substitute: it is end-to-end and meant for
                    // the origin, so accepting it here would be the credential-leak this design
                    // deliberately avoids.
                    "admin-style Authorization header instead",
                    head(&format!(
                        "Authorization: Basic {}\r\n",
                        encoded("ci:s3cr3t")
                    )),
                ),
            ];
            for (why, h) in cases {
                assert!(
                    !proxy_credential_matches(&h, &auth()),
                    "must fail closed: {why}"
                );
            }
        }

        // Headers land in a `HashMap`, so a duplicate overwrites: last wins. Pinned as intentional
        // — Rift is the terminal parser here, so this cannot be smuggled past a downstream hop, but
        // the behaviour should change deliberately rather than by accident.
        #[test]
        fn a_duplicate_header_takes_the_last_value() {
            let good = encoded("ci:s3cr3t");
            let bad = encoded("ci:wrong");
            let last_is_good = head(&format!(
                "Proxy-Authorization: Basic {bad}\r\nProxy-Authorization: Basic {good}\r\n"
            ));
            let last_is_bad = head(&format!(
                "Proxy-Authorization: Basic {good}\r\nProxy-Authorization: Basic {bad}\r\n"
            ));
            assert!(proxy_credential_matches(&last_is_good, &auth()));
            assert!(!proxy_credential_matches(&last_is_bad, &auth()));
        }

        #[test]
        fn header_name_is_case_insensitive() {
            let h = head(&format!(
                "PROXY-AUTHORIZATION: Basic {}\r\n",
                encoded("ci:s3cr3t")
            ));
            assert!(proxy_credential_matches(&h, &auth()));
        }

        #[test]
        fn a_blank_half_is_rejected_by_validate() {
            for (u, p) in [("", "p"), ("u", ""), ("  ", "p"), ("u", "  ")] {
                let a = InterceptAuth {
                    username: u.to_string(),
                    password: p.to_string(),
                };
                assert!(
                    a.validate().is_err(),
                    "a blank half enables the gate and then admits everyone: {u:?}/{p:?}"
                );
            }
            assert!(auth().validate().is_ok());
        }
    }

    #[test]
    fn parse_connect_accepts_authority() {
        let t = parse_connect(b"CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(t.host, "cdn.example.com");
        assert_eq!(t.port, 443);
    }

    #[test]
    fn parse_connect_defaults_port_and_rejects_malformed() {
        assert_eq!(parse_connect(b"CONNECT host\r\n\r\n").unwrap().port, 443);
        assert!(parse_connect(b"GET / HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_connect(b"CONNECT \r\n\r\n").is_none());
        assert!(parse_connect(b"\r\n\r\n").is_none());
        // A non-numeric port is a malformed request, not a default-to-443.
        assert!(parse_connect(b"CONNECT host:notaport HTTP/1.1\r\n\r\n").is_none());
    }

    // `connect_header` inherited the one job `parse_request_head` still had after #991: reading a
    // header out of the hand-read CONNECT head. Every way it can fail must yield `None`, because
    // `None` is what fails the #878 credential check closed.
    #[test]
    fn connect_header_reads_case_insensitively_and_fails_closed() {
        let head = b"CONNECT cdn.example.com:443 HTTP/1.1\r\nHost: cdn.example.com:443\r\nProxy-Authorization: Basic dTpw\r\n\r\n";
        assert_eq!(
            connect_header(head, "proxy-authorization").as_deref(),
            Some("Basic dTpw"),
            "the lookup is case-insensitive in the header name"
        );
        assert_eq!(connect_header(head, "x-absent"), None);

        // Last occurrence wins, matching the `HashMap` semantics `parse_request_head` had — the
        // behaviour `a_duplicate_header_takes_the_last_value` pins as deliberate.
        let duplicated = b"CONNECT h:443 HTTP/1.1\r\nX-Dup: first\r\nX-Dup: second\r\n\r\n";
        assert_eq!(
            connect_header(duplicated, "x-dup").as_deref(),
            Some("second")
        );

        assert_eq!(
            connect_header(b"not a request line at all\r\n\r\n", "proxy-authorization"),
            None,
            "an unparseable head yields no credential rather than a partial one"
        );

        let too_many: Vec<u8> = std::iter::once("CONNECT h:443 HTTP/1.1\r\n".to_string())
            .chain((0..MAX_CONNECT_HEADERS + 1).map(|i| format!("x-pad-{i}: v\r\n")))
            .chain(std::iter::once(
                "Proxy-Authorization: Basic dTpw\r\n\r\n".to_string(),
            ))
            .collect::<String>()
            .into_bytes();
        assert_eq!(
            connect_header(&too_many, "proxy-authorization"),
            None,
            "overflowing the header table fails closed, never open"
        );
    }

    async fn start_listener(rules: InterceptRules) -> (InterceptListener, String) {
        let ca = CertificateAuthority::generate().expect("ca");
        let ca_pem = ca.ca_cert_pem().to_string();
        let resolver = Arc::new(SniCertResolver::new(Arc::new(ca)));
        let listener =
            InterceptListener::bind("127.0.0.1:0".parse().unwrap(), resolver, rules, None)
                .await
                .expect("bind");
        (listener, ca_pem)
    }

    fn trusting_client(proxy_url: &str, ca_pem: &str) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(proxy_url).unwrap())
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn intercepts_https_via_connect_and_trusts_minted_leaf() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let proxy_url = format!("http://{}", listener.local_addr());

        // A client that trusts ONLY the intercept CA and routes HTTPS through the proxy. reqwest
        // issues CONNECT to the proxy, we MITM-terminate with a per-SNI leaf, and the client
        // validates that leaf against the CA it was handed.
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/config.json")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("cdn.example.com"),
            "response should echo the intercepted host, got: {body}"
        );

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn non_connect_request_is_rejected_without_panic() {
        let (listener, _ca_pem) = start_listener(InterceptRules::new()).await;
        let addr = listener.local_addr();

        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nhost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);
        assert!(text.starts_with("HTTP/1.1 405"), "got: {text}");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn tls_handshake_failure_is_handled_and_listener_survives() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let addr = listener.local_addr();

        // A client that CONNECTs, reads the 200, then sends non-TLS garbage. The server-side
        // handshake must fail without panicking or taking the listener down.
        {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"CONNECT cdn.example.com:443 HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            let mut buf = [0u8; 64];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"this is not a TLS ClientHello").await.unwrap();
            let _ = s.shutdown().await;
        }

        // The listener still serves a subsequent legitimate intercept.
        let proxy_url = format!("http://{addr}");
        let client = trusting_client(&proxy_url, &ca_pem);
        let resp = client
            .get("https://cdn.example.com/still-up")
            .send()
            .await
            .expect("listener should still serve after a failed handshake");
        assert_eq!(resp.status(), 200);

        listener.shutdown().await;
    }

    /// A client-side TLS connector that trusts only the intercept CA (issue #1010).
    ///
    /// The provider is passed explicitly rather than relying on a process-wide default: these
    /// tests can run before anything installs one, and the failure mode there is a panic inside
    /// rustls rather than a readable assertion.
    fn tls_client_connector(ca_pem: &str) -> tokio_rustls::TlsConnector {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
            roots
                .add(cert.expect("ca cert parses"))
                .expect("ca trusted");
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("client config")
        .with_root_certificates(roots)
        .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    /// Issue #1010: an established tunnel that has not yet sent a request must be closed by
    /// `shutdown()`, not left to expire on its own.
    ///
    /// Before this change the shutdown signal reached only the accept loop, so such a socket
    /// survived until `header_read_timeout` (30s by default) — `shutdown()` returned while the
    /// listener still held it. The bound below is far under that timeout, so this fails on the
    /// old behaviour rather than merely being slow.
    #[tokio::test]
    async fn shutdown_closes_a_tunnel_awaiting_its_first_request() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let addr = listener.local_addr();

        let mut sock = TcpStream::connect(addr).await.unwrap();
        sock.write_all(b"CONNECT cdn.example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut head = [0u8; 128];
        let n = sock.read(&mut head).await.unwrap();
        assert!(
            String::from_utf8_lossy(&head[..n]).starts_with("HTTP/1.1 200"),
            "CONNECT should be accepted before the tunnel exists"
        );

        // A completed handshake is what puts the connection inside `serve_tunnel`, which is the
        // only place the drain can act. Stopping at CONNECT would test nothing.
        let server_name = rustls::pki_types::ServerName::try_from("cdn.example.com")
            .unwrap()
            .to_owned();
        let mut tls = tls_client_connector(&ca_pem)
            .connect(server_name, sock)
            .await
            .expect("client handshake against the minted leaf");

        listener.shutdown().await;

        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_secs(5), tls.read(&mut byte)).await {
            // EOF, or a transport-level close — either is the tunnel being closed.
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(_)) => panic!("an idle tunnel answered bytes after shutdown"),
            Err(_) => {
                panic!("shutdown() returned but the idle tunnel was still open five seconds later")
            }
        }
    }

    /// The other half, and the one that separates a graceful drain from an abort: a request that
    /// is already in flight when `shutdown()` lands must still get its complete response.
    ///
    /// Without this, "close every tunnel on shutdown" would pass the test above while cutting
    /// live requests off mid-response.
    #[tokio::test]
    async fn shutdown_lets_an_in_flight_request_complete() {
        let (received_tx, received_rx) = tokio::sync::oneshot::channel();

        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = upstream.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                // Tell the test the request is genuinely in flight, then stall long enough that
                // `shutdown()` is guaranteed to land while it still is.
                let _ = received_tx.send(());
                tokio::time::sleep(Duration::from_millis(400)).await;
                let body = "drained";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget {
                    port: upstream_port,
                }),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let request = tokio::spawn(async move {
            client
                .get("https://cdn.example.com/slow")
                .send()
                .await
                .expect("in-flight request must survive shutdown")
                .text()
                .await
                .expect("body must arrive complete")
        });

        received_rx.await.expect("upstream received the request");
        listener.shutdown().await;

        let body = tokio::time::timeout(Duration::from_secs(10), request)
            .await
            .expect("the in-flight request must not hang")
            .expect("request task");
        assert_eq!(
            body, "drained",
            "shutdown must drain the in-flight request, not abort it"
        );
    }

    #[tokio::test]
    async fn serve_rule_returns_inline_stub() {
        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: Some("cdn.example.com".to_string()),
                predicates: vec![],
                action: InterceptAction::Serve(ServeStub::new(
                    418,
                    HashMap::from([("x-rift".to_string(), vec!["1".to_string()])]),
                    Some(serde_json::json!("brewed")),
                )),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/x")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 418);
        assert_eq!(resp.headers().get("x-rift").unwrap(), "1");
        let body = resp.text().await.unwrap();
        assert!(body.contains("brewed"), "got: {body}");

        listener.shutdown().await;
    }

    // Issue #646, call-site guard: drives the live listener path, so it fails if
    // `handle_connection` is ever rewired to hand the lossy string (rather than the classified
    // base64 one) to rule matching — the helper's own unit tests cannot catch that.
    #[tokio::test]
    async fn binary_body_matches_base64_keyed_rule_through_live_listener() {
        let binary: &[u8] = &[0x1F, 0x8B, 0x08, 0x00, 0xFF, 0xFE];
        let b64 = base64::engine::general_purpose::STANDARD.encode(binary);
        assert_ne!(
            b64,
            String::from_utf8_lossy(binary),
            "the two conventions must be distinguishable"
        );

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![
                    serde_json::from_value(serde_json::json!({ "equals": { "body": b64 } }))
                        .expect("valid predicate JSON"),
                ],
                action: InterceptAction::Serve(ServeStub::new(
                    418,
                    HashMap::new(),
                    Some(serde_json::json!("matched-binary")),
                )),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .post("https://cdn.example.com/upload")
            .body(binary.to_vec())
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(
            resp.status(),
            418,
            "the base64-keyed body predicate must match the raw binary body \
             (a lossy-classified body falls through to the 200 echo)"
        );

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn forward_rule_proxies_to_imposter_port() {
        // A trivial local HTTP server standing in for an imposter.
        let imposter = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let imposter_port = imposter.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = imposter.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let body = "from-imposter";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.shutdown().await;
            }
        });

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget {
                    port: imposter_port,
                }),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/anything")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "from-imposter");

        listener.shutdown().await;
    }

    /// The forward client must be built once per listener, not per request (issue #552): a fresh
    /// `reqwest::Client` has an empty connection pool, so every forward would open a new TCP
    /// connection to the imposter and keep-alive would never engage.
    ///
    /// Counts connections *accepted* by a keep-alive imposter across N sequential forwards, which
    /// is the only way to observe pooling from the outside. A per-request client yields N accepts;
    /// a shared client yields 1. Note the imposter here must serve many requests per connection —
    /// `forward_rule_proxies_to_imposter_port`'s single-shot imposter would mask this entirely.
    #[tokio::test]
    async fn forward_client_is_reused_across_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        const FORWARDS: usize = 4;

        let imposter = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let imposter_port = imposter.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));

        let accepts_srv = Arc::clone(&accepts);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = imposter.accept().await {
                accepts_srv.fetch_add(1, Ordering::SeqCst);
                // Serve every request on this connection, keep-alive (no `connection: close`), so
                // a pooled client can legitimately reuse it.
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let body = "from-imposter";
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                                    body.len()
                                );
                                if s.write_all(resp.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget {
                    port: imposter_port,
                }),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        for i in 0..FORWARDS {
            let resp = client
                .get("https://cdn.example.com/anything")
                .send()
                .await
                .unwrap_or_else(|e| panic!("forward {i} intercepted: {e}"));
            assert_eq!(resp.status(), 200, "forward {i}");
            // Drain the body so the forward connection returns to the pool.
            assert_eq!(resp.text().await.unwrap(), "from-imposter", "forward {i}");
        }

        let observed = accepts.load(Ordering::SeqCst);
        assert_eq!(
            observed, 1,
            "{FORWARDS} forwards must reuse ONE pooled connection to the imposter; saw {observed} \
             accepts (a fresh client per request would open {FORWARDS})"
        );

        listener.shutdown().await;
    }

    /// A forward to a dead port must not poison the now-shared client for later forwards (issue
    /// #552). With a per-request client this was impossible by construction; sharing one makes it
    /// worth pinning, since a connect error must stay scoped to its own request.
    #[tokio::test]
    async fn forward_error_does_not_poison_client_for_later_requests() {
        let imposter = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = imposter.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = imposter.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let body = "alive";
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                                    body.len()
                                );
                                if s.write_all(resp.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        // A port nothing can be listening on — see the note on CLOSED_PORT below (issue #859).
        // Bind-then-drop returns the number to the ephemeral allocator, so another test in this
        // binary can take it and turn "dead upstream" into a live one.
        let dead_port = CLOSED_PORT;

        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: Some("dead.example.com".to_string()),
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget { port: dead_port }),
            })
            .unwrap();
        rules
            .add(InterceptRule {
                host: Some("live.example.com".to_string()),
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget { port: live_port }),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let dead = client
            .get("https://dead.example.com/x")
            .send()
            .await
            .expect("intercepted");
        assert_eq!(dead.status(), 502, "dead port must relay 502");

        // The same shared forward client must still serve a healthy target afterwards.
        let live = client
            .get("https://live.example.com/x")
            .send()
            .await
            .expect("intercepted");
        assert_eq!(live.status(), 200, "client must survive an earlier 502");
        assert_eq!(live.text().await.unwrap(), "alive");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn no_matching_rule_falls_back_to_default() {
        let (listener, ca_pem) = start_listener(InterceptRules::new()).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/whatever")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("cdn.example.com"), "got: {body}");

        listener.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_forward_port_returns_502() {
        let rules = InterceptRules::new();
        rules
            .add(InterceptRule {
                host: None,
                predicates: vec![],
                action: InterceptAction::Forward(ForwardTarget { port: CLOSED_PORT }),
            })
            .unwrap();
        let (listener, ca_pem) = start_listener(rules).await;
        let proxy_url = format!("http://{}", listener.local_addr());
        let client = trusting_client(&proxy_url, &ca_pem);

        let resp = client
            .get("https://cdn.example.com/x")
            .send()
            .await
            .expect("request intercepted");
        assert_eq!(resp.status(), 502);

        listener.shutdown().await;
    }
}
