//! The HTTP side of the steps: one [`Clients`] set per run, and [`Response`], what the engine
//! served for the last request.

use crate::HarnessError;
use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

/// Mirrors the Scala runner's 10s request timeout: a wedged engine fails the scenario rather than
/// hanging the run.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Install ring as the process's rustls provider, as every Rift entry point does. reqwest is built
/// with `rustls-no-provider`, so without this no TLS client can be built. A provider that is
/// already installed (embedded mode's `rift_start` installs the same one) is what this wants, so
/// that outcome is not an error.
pub fn install_crypto_provider() {
    // `Err` carries the provider that was already installed; either way one is in place.
    let _already_installed = rustls::crypto::ring::default_provider().install_default();
}

/// The clients the steps send with. Every one ignores `HTTP(S)_PROXY`: the engine is on loopback,
/// and the Scala runner's JDK client never consulted the environment either.
#[derive(Debug, Clone)]
pub struct Clients {
    /// Plain HTTP, and HTTPS against the platform trust store (the "cert-verifying" client).
    pub verifying: reqwest::Client,
    /// HTTPS trusting any server certificate: the imposters serve rift's self-signed default.
    pub insecure: reqwest::Client,
}

impl Clients {
    pub fn new() -> Result<Self, HarnessError> {
        Ok(Self {
            verifying: base().build().map_err(HarnessError::Client)?,
            insecure: base()
                .tls_danger_accept_invalid_certs(true)
                .build()
                .map_err(HarnessError::Client)?,
        })
    }

    /// An HTTPS client presenting `rift-fixtures/tls/<name>-cert.pem` + `<name>-key.pem` as its
    /// client certificate, trusting any server certificate (the scenarios are about the client's
    /// certificate; the imposter serves rift's self-signed default).
    pub fn mtls(fixtures: &Path, name: &str) -> Result<reqwest::Client, HarnessError> {
        let read = |file: String| {
            let path = fixtures.join("tls").join(file);
            std::fs::read(&path).map_err(|source| HarnessError::Io {
                action: "reading",
                path,
                source,
            })
        };
        let mut pem = read(format!("{name}-cert.pem"))?;
        pem.push(b'\n');
        pem.extend(read(format!("{name}-key.pem"))?);
        let identity = reqwest::Identity::from_pem(&pem).map_err(HarnessError::Client)?;
        base()
            .tls_danger_accept_invalid_certs(true)
            .identity(identity)
            .build()
            .map_err(HarnessError::Client)
    }
}

fn base() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .tls_backend_rustls()
}

/// What the engine served. Header names are lower-cased, so lookups are case-insensitive.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub body: String,
    pub headers: BTreeMap<String, Vec<String>>,
}

impl Response {
    /// Read a whole reqwest response. The body is decoded as UTF-8 (lossily: an assertion on a
    /// non-UTF-8 body compares against the replacement characters and fails visibly).
    pub async fn read(resp: reqwest::Response) -> Result<Self, reqwest::Error> {
        let status = resp.status().as_u16();
        let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, value) in resp.headers() {
            headers
                .entry(name.as_str().to_ascii_lowercase())
                .or_default()
                .push(String::from_utf8_lossy(value.as_bytes()).into_owned());
        }
        let body = String::from_utf8_lossy(&resp.bytes().await?).into_owned();
        Ok(Self {
            status,
            body,
            headers,
        })
    }

    #[must_use]
    pub fn header_values(&self, name: &str) -> &[String] {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map_or(&[], Vec::as_slice)
    }
}

/// How a request that was expected to be refused at the TLS layer actually failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// The TLS layer refused it: a rustls error (a bad certificate, an alert from the peer), the
    /// peer tearing the connection down mid-handshake, or a timeout. What the refusal steps want.
    TlsRefused(String),
    /// The request never reached a TLS peer: nothing listening (connection refused) or the host
    /// did not resolve. A dead engine or an unbound imposter, never a refusal; the Scala runner
    /// rethrows these as defects for the same reason.
    Unreachable(String),
    /// Anything else (a malformed URL, a client-side builder error): not evidence of a refusal.
    Other(String),
}

/// The next link of an error chain. A custom `io::Error`'s `source()` skips the error it wraps (it
/// returns that error's own source), and the client stack nests one `io::Error` in another around
/// the rustls error, so an `io::Error` is followed through `get_ref()` instead.
fn next<'a>(e: &'a (dyn StdError + 'static)) -> Option<&'a (dyn StdError + 'static)> {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) => io.get_ref().map(|inner| inner as &(dyn StdError + 'static)),
        None => e.source(),
    }
}

/// Classify a failed request by walking its `source()` chain. Pure, so it is unit-tested.
#[must_use]
pub fn classify_failure(err: &(dyn StdError + 'static)) -> FailureKind {
    let chain: Vec<&(dyn StdError + 'static)> =
        std::iter::successors(Some(err), |&e| next(e)).collect();
    let text = chain
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    let io_kinds: Vec<ErrorKind> = chain
        .iter()
        .filter_map(|e| e.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind)
        .collect();
    let dns = chain.iter().any(|e| {
        let t = e.to_string();
        t.contains("dns error") || t.contains("failed to lookup address")
    });
    if io_kinds.contains(&ErrorKind::ConnectionRefused) {
        return FailureKind::Unreachable(format!("connection refused (nothing listening): {text}"));
    }
    if dns {
        return FailureKind::Unreachable(format!("host did not resolve: {text}"));
    }
    let timed_out = io_kinds.contains(&ErrorKind::TimedOut)
        || chain
            .iter()
            .filter_map(|e| e.downcast_ref::<reqwest::Error>())
            .any(reqwest::Error::is_timeout);
    let tls = chain.iter().any(|e| {
        e.downcast_ref::<rustls::Error>().is_some()
            || e.downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .is_some_and(|inner| inner.downcast_ref::<rustls::Error>().is_some())
    });
    // After a TCP connect succeeded, the peer aborting the handshake surfaces as one of these.
    let torn_down = io_kinds.iter().any(|k| {
        matches!(
            k,
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionAborted
                | ErrorKind::UnexpectedEof
                | ErrorKind::BrokenPipe
        )
    });
    // TLS 1.3: the client finishes its side of the handshake before the server has checked the
    // client certificate, so a server refusing it closes a connection the client already holds.
    // hyper then cancels the request it had queued on it ("connection was not ready"). Reaching
    // this point means the connect itself succeeded (refused and DNS returned above), so this is
    // the peer tearing the TLS session down, not an unreachable engine.
    let closed_by_peer = chain.iter().any(|e| {
        let t = e.to_string();
        t.contains("connection was not ready")
            || t.contains("connection closed before message completed")
    });
    if tls || timed_out || torn_down || closed_by_peer {
        FailureKind::TlsRefused(text)
    } else {
        FailureKind::Other(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Wrap(&'static str, std::io::Error);
    impl std::fmt::Display for Wrap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl StdError for Wrap {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.1)
        }
    }

    #[test]
    fn connection_refused_is_unreachable_not_a_refusal() {
        let e = Wrap(
            "client error (Connect)",
            ErrorKind::ConnectionRefused.into(),
        );
        assert!(matches!(classify_failure(&e), FailureKind::Unreachable(_)));
    }

    #[test]
    fn a_dns_failure_is_unreachable() {
        let e = Wrap(
            "dns error",
            std::io::Error::other("failed to lookup address information"),
        );
        assert!(matches!(classify_failure(&e), FailureKind::Unreachable(_)));
    }

    #[test]
    fn a_rustls_error_inside_an_io_error_is_a_refusal() {
        let io = std::io::Error::new(
            ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::CertificateRequired),
        );
        let e = Wrap("client error (Connect)", io);
        assert!(matches!(classify_failure(&e), FailureKind::TlsRefused(_)));
    }

    /// The shape reqwest 0.13 really produces for an untrusted server certificate: the rustls
    /// error inside an `io::Error` inside another `io::Error`, whose `source()` skips both.
    #[test]
    fn a_rustls_error_two_io_errors_deep_is_a_refusal() {
        let inner = std::io::Error::new(
            ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        );
        let e = Wrap("client error (Connect)", std::io::Error::other(inner));
        assert!(matches!(classify_failure(&e), FailureKind::TlsRefused(_)));
    }

    #[test]
    fn a_reset_mid_handshake_and_a_timeout_are_refusals() {
        let reset = Wrap("tls handshake", ErrorKind::ConnectionReset.into());
        assert!(matches!(
            classify_failure(&reset),
            FailureKind::TlsRefused(_)
        ));
        let timeout = Wrap("operation timed out", ErrorKind::TimedOut.into());
        assert!(matches!(
            classify_failure(&timeout),
            FailureKind::TlsRefused(_)
        ));
    }

    #[test]
    fn a_session_the_peer_closed_after_the_handshake_is_a_refusal() {
        let e = Wrap(
            "client error (Canceled)",
            std::io::Error::other("operation was canceled: connection was not ready"),
        );
        assert!(matches!(classify_failure(&e), FailureKind::TlsRefused(_)));
    }

    /// End to end through reqwest's real error chain: a port with nothing listening.
    #[tokio::test]
    async fn a_real_connection_refused_from_reqwest_is_unreachable() {
        install_crypto_provider();
        let port = crate::harness::free_port().expect("a free port");
        let client = Clients::new().expect("clients");
        let err = client
            .insecure
            .get(format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .expect_err("nothing listens there");
        let kind = classify_failure(&err);
        assert!(matches!(kind, FailureKind::Unreachable(_)), "{kind:?}");
    }

    #[test]
    fn anything_else_is_not_a_refusal() {
        let e = Wrap(
            "builder error",
            std::io::Error::other("relative URL without a base"),
        );
        assert!(matches!(classify_failure(&e), FailureKind::Other(_)));
    }
}
