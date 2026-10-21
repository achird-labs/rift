//! The response behavior pipeline: a response's behaviors program — every step, in order (issue
//! #1198) — applied to its status, headers and body.
//!
//! One function so every response type that carries behaviors runs the same pipeline with the
//! same failure contract (issue #1184): the `is`, `inject` and `proxy` serve paths call it.

use super::handler::SCRIPT_TIMEOUT_HEADER;
use super::headers::StubRef;
use super::response::{apply_decorate_bounded, body_text};
use super::types::{IsResponse, StatusValue};
use crate::behaviors::{
    BehaviorProgram, BehaviorStep, CsvCache, Decorated, RequestContext, ResponseStatus,
    ShellTransformError, Spliced, SplicedHeaders, WaitScriptError, apply_copy_spliced,
    apply_lookup_spliced, apply_shell_transform,
};
use crate::util::build_response_with_headers;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use std::cell::OnceCell;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::time::Duration;
use tracing::warn;

/// The parts of a response the pipeline reads and rewrites.
///
/// Body and header values carry which bytes the engine substituted in (issue #1203), so `copy` and
/// `lookup` never expand a token that arrived inside request text.
#[derive(Debug)]
pub(crate) struct ServedParts {
    /// The status, unless `status_text` is set.
    pub status: u16,
    /// A `statusCode` string that is not a number (issue #1558): `copy` and `lookup` substitute
    /// into it, `decorate` and `shellTransform` see it as a string, and it is read when the
    /// response is written ([`super::status::resolve`]).
    pub status_text: Option<Spliced>,
    /// Multi-value (issue #238): one entry per header name, one element per header line.
    pub headers: SplicedHeaders,
    pub body: Spliced,
}

impl ServedParts {
    /// A response a script wrote, completed from the imposter's `defaultResponse` (issue #1369)
    /// or, without one, from the built-in `200`, no headers and an empty body. What the script
    /// wrote is its own text (issue #1203); a string status taken from the default is supplied
    /// whole, since Mountebank merges the default after the behaviors (issue #1558).
    pub(crate) fn completed(authored: &IsResponse, default: Option<&IsResponse>) -> Self {
        let fallback = IsResponse::default();
        let completed = authored.completed_by(default.unwrap_or(&fallback));
        let status_text = completed.status_text.as_deref().map(|text| {
            if authored.omitted.status_code {
                Spliced::supplied(text.to_string())
            } else {
                Spliced::authored(text.to_string())
            }
        });
        ServedParts {
            status: completed.status_code,
            status_text,
            headers: completed
                .headers
                .iter()
                .map(|(k, values)| {
                    (
                        k.clone(),
                        values
                            .iter()
                            .map(|v| Spliced::authored(v.clone()))
                            .collect(),
                    )
                })
                .collect(),
            body: Spliced::authored(body_text(completed.body.as_ref())),
        }
    }
}

/// What running the pipeline produced. A dropped `StrictFailure` would serve the response the
/// strict contract refused, so the outcome must be matched.
#[derive(Debug)]
#[must_use]
pub(crate) enum BehaviorOutcome {
    /// The response to serve. `degraded` is true when a behavior failed under the lenient
    /// contract (#269/#323): the response is served, but carries a signal header and is not the
    /// response the configuration asked for.
    Applied { parts: ServedParts, degraded: bool },
    /// A behavior failed under `strictBehaviors` (#375), or failed in a way Mountebank always
    /// answers with an error, as a `wait` function that throws (#1616): serve this error response
    /// instead.
    StrictFailure(Response<Full<Bytes>>),
    /// The imposter was torn down while a `wait` was sleeping (issue #1616): close the connection
    /// with no response, with [`aborted_response`].
    Aborted,
}

/// The response for [`BehaviorOutcome::Aborted`]: a `TcpFaultKind::Empty` carrier, so the listener
/// closes the connection without writing a byte (issue #1616), the one mechanism that already
/// aborts a connection from a response. Over HTTP/1 the client sees the connection close before a
/// response, as when Mountebank destroys an imposter's sockets; over HTTP/2 the imposter's
/// listener aborts the whole connection, whose streams all belong to the deleted imposter, and
/// the `/__rift/` gateway and the front door reset the one stream. An in-process caller sees the
/// carrier, which [`tcp_fault_carrier`](super::fault_io::tcp_fault_carrier) names
/// `EMPTY_RESPONSE`.
pub(crate) fn aborted_response() -> Response<Full<Bytes>> {
    let mut response = build_response_with_headers(
        StatusCode::SERVICE_UNAVAILABLE,
        [("x-rift-imposter", "true")],
        Bytes::new(),
    );
    response
        .extensions_mut()
        .insert(super::fault_io::TcpFaultKind::Empty);
    response
}

/// Mountebank's answer to a `wait` function that throws (issue #1616): `500` with
/// `{"errors":[{"code":"invalid injection","message":"invalid wait injection","data":...}]}`, as
/// its HTTP server writes an `InjectionError`. `data` is the thrown message, left out when the
/// thrown value has none. `type` reuses the frozen `invalid injection` slug (#797); `source`,
/// `name` and `stack` are left out, as on every Rift error envelope.
fn wait_error_response(e: &WaitScriptError) -> Response<Full<Bytes>> {
    let kind = crate::response::ErrorKind::InvalidInjection.slug();
    let mut error = serde_json::json!({
        "code": kind,
        "type": kind,
        "message": "invalid wait injection",
    });
    if let Some(data) = &e.data {
        error["data"] = data.as_str().into();
    }
    build_response_with_headers(
        StatusCode::INTERNAL_SERVER_ERROR,
        [
            ("x-rift-imposter", "true"),
            ("x-rift-wait-error", "true"),
            ("content-type", "application/json"),
        ],
        serde_json::json!({ "errors": [error] }).to_string(),
    )
}

/// Everything the pipeline reads besides the response itself.
pub(crate) struct BehaviorRun<'a, SH> {
    pub behaviors: &'a BehaviorProgram,
    pub method: &'a str,
    pub uri: &'a hyper::Uri,
    pub request_headers: &'a HashMap<String, Vec<String>, SH>,
    pub request_body: Option<&'a str>,
    /// The client's socket, for `requestFrom` / `ip` in `decorate` and `MB_REQUEST` (issue #1451).
    /// `None` only for a caller with no socket.
    pub client_addr: Option<std::net::SocketAddr>,
    pub script_state_key: u16,
    /// What the imposter's scripts may `require()` (issue #1611).
    pub script_files: std::sync::Arc<crate::imposter::ScriptFileAccess>,
    pub stub: StubRef<'a>,
    pub csv_cache: &'a CsvCache,
    pub script_timeout: Duration,
    pub strict: bool,
    /// The imposter's `defaultResponse`: it completes a response a `decorate` returned (issue
    /// #1567), as Mountebank completes every response it serves.
    pub default_response: Option<&'a IsResponse>,
    /// The imposter's teardown signal, which cuts a sleeping `wait` off (issue #1616). `None` for
    /// an imposter that was never started by a manager.
    pub shutdown: Option<&'a tokio::sync::broadcast::Sender<()>>,
}

/// The response's headers as a script sees them in `MB_RESPONSE` (issue #1589): one string per
/// header, an array for a header with several lines.
fn headers_json(headers: &SplicedHeaders) -> serde_json::Value {
    headers
        .iter()
        .map(|(name, lines)| {
            let value = match lines.as_slice() {
                [line] => line.as_str().into(),
                lines => lines.iter().map(Spliced::as_str).collect(),
            };
            (name.clone(), value)
        })
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// A shellTransform's stdout as the response it replaces this one with (issue #1589): an object
/// is read as an `is` response, and any other value is an empty one, as Mountebank reads no field
/// of it. What the response leaves out is the default's.
fn shell_response(stdout: serde_json::Value) -> Result<IsResponse, ShellTransformError> {
    if stdout.is_object() {
        serde_json::from_value(stdout)
            .map_err(|e| ShellTransformError::InvalidResponse(e.to_string()))
    } else {
        Ok(IsResponse::empty())
    }
}

impl<SH: BuildHasher> BehaviorRun<'_, SH> {
    /// Run the program's steps over `parts`, in order (issue #1198). A step that fails under the
    /// lenient contract marks the response degraded and the next step still runs; under
    /// `strictBehaviors` the first failure is the response.
    pub(crate) async fn apply(&self, parts: ServedParts) -> BehaviorOutcome {
        let ServedParts {
            mut status,
            mut status_text,
            mut headers,
            mut body,
        } = parts;
        // What `decorate` and `shellTransform` see as `statusCode`, as Mountebank hands it over.
        let script_status = |status: u16, text: &Option<Spliced>| match text {
            Some(text) => ResponseStatus::Text(text.as_str().to_string()),
            None => ResponseStatus::Code(status),
        };
        let mut degraded = false;
        // The JSON body the last shellTransform returned (issue #1589), so the next command reads
        // a number as a number, as in Mountebank. Used only while the body is still its rendering:
        // any step that rewrites the body leaves text.
        let mut shell_body: Option<serde_json::Value> = None;

        // Lazy request context (issue #561): only copy/lookup/decorate/shellTransform read
        // it, and `RequestContext::from_request` re-parses the query, clones a key and
        // one value per header, and copies the body — so a wait/repeat-only stub must not pay
        // for it.
        //
        // Built on first read rather than behind a hand-maintained "does anything below
        // need this?" predicate: such a predicate has to be kept in sync with consumers
        // 100+ lines away, and getting it wrong would hand one an empty-but-valid context —
        // wrong output, no error. Here a consumer that forgets to ask simply cannot exist.
        let request_context: OnceCell<RequestContext> = OnceCell::new();
        let build_request_context = || {
            RequestContext::from_request(
                self.method,
                self.uri,
                self.request_headers,
                self.request_body,
                self.client_addr,
            )
        };

        // copy/lookup are pure token substitution — apply them across each value of
        // multi-value headers so multiplicity survives (e.g. multiple Set-Cookie;
        // RFC 7230 §3.2.2 forbids folding Set-Cookie). decorate uses a single-value
        // JS/Rhai object model, so only that path collapses — and even there Set-Cookie
        // is held aside, never comma-folded.
        for step in &self.behaviors.steps {
            match step {
                BehaviorStep::Wait(wait) => {
                    // A wait function that throws is the response (issue #1616), with or without
                    // `strictBehaviors`: Mountebank has no lenient mode for it.
                    let wait_ms = match wait.get_duration_ms() {
                        Ok(ms) => ms,
                        Err(e) => return BehaviorOutcome::StrictFailure(wait_error_response(&e)),
                    };
                    if wait_ms > 0 {
                        let sleep = tokio::time::sleep(Duration::from_millis(wait_ms));
                        // Raced against the imposter's teardown (issue #1616): a delete cuts the
                        // wait off, as Mountebank destroys the socket, instead of answering from
                        // the deleted imposter when the wait ends. Subscribed when the wait
                        // starts, so a delete that lands between the request's arrival and this
                        // point is missed: that request sleeps on, and the delete's bounded drain
                        // (#596) still caps how long it waits for it.
                        match self.shutdown.map(tokio::sync::broadcast::Sender::subscribe) {
                            Some(mut shutdown) => tokio::select! {
                                () = sleep => {}
                                // `Err(Closed)` means the imposter's sender is gone, which is a
                                // teardown too.
                                _ = shutdown.recv() => return BehaviorOutcome::Aborted,
                            },
                            None => sleep.await,
                        }
                    }
                }
                BehaviorStep::Copy(copy) => {
                    apply_copy_spliced(
                        &mut body,
                        &mut headers,
                        &mut status_text,
                        std::slice::from_ref(copy),
                        request_context.get_or_init(build_request_context),
                        self.stub,
                    );
                }
                BehaviorStep::Lookup(lookup) => {
                    apply_lookup_spliced(
                        &mut body,
                        &mut headers,
                        &mut status_text,
                        std::slice::from_ref(lookup),
                        request_context.get_or_init(build_request_context),
                        self.csv_cache,
                        &self.script_files,
                        self.stub,
                    );
                }
                BehaviorStep::Decorate(decorate_script) => {
                    // decorate uses a single-value JS/Rhai object model. Set-Cookie is held
                    // aside and never folded (RFC 7230 §3.2.2); other multi-value headers
                    // degrade to single-value for the script (issue #238 boundary) — warn so
                    // the collapse is not silent (e.g. WWW-Authenticate is also corrupted by
                    // comma-folding).
                    let is_set_cookie = |k: &str| k.eq_ignore_ascii_case("set-cookie");
                    let folded: Vec<&String> = headers
                        .iter()
                        .filter(|(k, v)| v.len() > 1 && !is_set_cookie(k))
                        .map(|(k, _)| k)
                        .collect();
                    if !folded.is_empty() {
                        warn!(
                            "decorate uses a single-value object model; multi-value headers \
                                 {folded:?} are comma-folded (issue #238 boundary). Set-Cookie is \
                                 exempt; other headers that forbid list-folding will be corrupted."
                        );
                    }

                    let set_cookie: Vec<(String, Vec<Spliced>)> = headers
                        .iter()
                        .filter(|(k, _)| is_set_cookie(k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    let single: HashMap<String, String> = headers
                        .iter()
                        .filter(|(k, _)| !is_set_cookie(k))
                        .map(|(k, v)| {
                            let lines: Vec<&str> = v.iter().map(Spliced::as_str).collect();
                            (k.clone(), lines.join(", "))
                        })
                        .collect();
                    match apply_decorate_bounded(
                        decorate_script.clone(),
                        request_context.get_or_init(build_request_context).clone(),
                        body.as_str().to_string(),
                        script_status(status, &status_text),
                        single,
                        self.script_state_key,
                        self.stub.id.map(str::to_owned),
                        std::sync::Arc::clone(&self.script_files),
                        self.script_timeout,
                    )
                    .await
                    {
                        // A returned object replaces the response (issue #1567). Mountebank keeps
                        // nothing of the original, Set-Cookie included, and completes what the
                        // object leaves out from `defaultResponse`.
                        Ok(Decorated {
                            status_code,
                            headers: single,
                            body: new_body,
                            returned: Some(omits),
                        }) => {
                            let authored = IsResponse::decoded(
                                (!omits.status_code).then(|| match status_code {
                                    ResponseStatus::Code(code) => StatusValue::Code(code),
                                    ResponseStatus::Text(text) => StatusValue::Text(text.into()),
                                }),
                                (!omits.headers).then(|| {
                                    single.into_iter().map(|(k, v)| (k, vec![v])).collect()
                                }),
                                (!omits.body).then_some(serde_json::Value::String(new_body)),
                                None,
                            );
                            let completed =
                                ServedParts::completed(&authored, self.default_response);
                            status = completed.status;
                            status_text = completed.status_text;
                            headers = completed.headers;
                            body = completed.body;
                        }
                        // A script's output is the author's text (issue #1203): ranges cannot
                        // survive an arbitrary rewrite, and the script is configuration.
                        Ok(Decorated {
                            status_code: new_status,
                            headers: single,
                            body: new_body,
                            returned: None,
                        }) => {
                            body = Spliced::authored(new_body);
                            match new_status {
                                ResponseStatus::Code(code) => {
                                    status = code;
                                    status_text = None;
                                }
                                ResponseStatus::Text(text) => {
                                    status_text = Some(Spliced::authored(text));
                                }
                            }
                            // Restore the held-aside Set-Cookie lines unless the script set its
                            // own (case-insensitively) — a script override wins deterministically.
                            let script_set_cookie = single.keys().any(|k| is_set_cookie(k));
                            headers = single
                                .into_iter()
                                .map(|(k, v)| (k, vec![Spliced::authored(v)]))
                                .collect();
                            if !script_set_cookie {
                                headers.extend(set_cookie);
                            }
                        }
                        // Behave as if decorate was absent: keep the original multi-value
                        // `headers` and pre-decorate body/status rather than serving a folded,
                        // undecorated response. Attach a visible signal so the skipped behavior
                        // isn't a silent success (issue #323); the body is still served (#269).
                        Err(e) => {
                            warn!("Decorate script error: {e}");
                            // A deadline miss (issue #499) carries `x-rift-script-timeout` and,
                            // under strict mode, a 504 rather than the broken-script 500 — so a
                            // retry-worthy timeout is distinguishable from a permanent failure.
                            let timed_out =
                                matches!(e, crate::behaviors::DecorateError::Timeout(_));
                            if self.strict {
                                let status = if timed_out {
                                    StatusCode::GATEWAY_TIMEOUT
                                } else {
                                    StatusCode::INTERNAL_SERVER_ERROR
                                };
                                let mut hdrs = vec![
                                    ("x-rift-imposter", "true"),
                                    ("x-rift-decorate-error", "true"),
                                    ("content-type", "application/json"),
                                ];
                                if timed_out {
                                    hdrs.push((SCRIPT_TIMEOUT_HEADER, "true"));
                                }
                                return BehaviorOutcome::StrictFailure(
                                    build_response_with_headers(
                                        status,
                                        hdrs,
                                        crate::response::error_body_typed(
                                            status,
                                            crate::response::ErrorKind::BehaviorError,
                                            &format!("decorate failed (strictBehaviors): {e}"),
                                        ),
                                    ),
                                );
                            }
                            degraded = true;
                            headers.insert(
                                "x-rift-decorate-error".to_string(),
                                vec![Spliced::authored("true".to_string())],
                            );
                            if timed_out {
                                headers.insert(
                                    SCRIPT_TIMEOUT_HEADER.to_string(),
                                    vec![Spliced::authored("true".to_string())],
                                );
                            }
                        }
                    }
                }
                // shellTransform (issues #269, #1589): Mountebank's contract. The command gets the
                // request and the response as `MB_REQUEST` / `MB_RESPONSE` and as two arguments,
                // and its stdout is a JSON response that replaces this one, completed from
                // `defaultResponse` like a returned decorate object (#1567).
                BehaviorStep::ShellTransform(cmd) => {
                    let response_in = serde_json::json!({
                        "statusCode": script_status(status, &status_text).to_json(),
                        "headers": headers_json(&headers),
                        "body": shell_body
                            .as_ref()
                            .filter(|value| body_text(Some(value)) == body.as_str())
                            .cloned()
                            .unwrap_or_else(|| body.as_str().into()),
                    });
                    // Run the fork/exec/wait off the tokio worker (issue #478): a synchronous
                    // subprocess run inline would stall the worker for its whole lifetime,
                    // starving unrelated requests multiplexed on it.
                    let shell_result = {
                        let cmd = cmd.clone();
                        let rc = request_context.get_or_init(build_request_context).clone();
                        // Plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987):
                        // `apply_shell_transform` is a fork/exec of an external command and never
                        // touches a `FlowStore`, so nothing here can annotate.
                        tokio::task::spawn_blocking(move || {
                            apply_shell_transform(&cmd, &rc, &response_in)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            Err(ShellTransformError::Spawn(format!(
                                "shellTransform task panicked: {e}"
                            )))
                        })
                        .and_then(shell_response)
                    };
                    match shell_result {
                        Ok(authored) => {
                            let fallback = IsResponse::default();
                            let default = self.default_response.unwrap_or(&fallback);
                            shell_body.clone_from(&authored.completed_by(default).body);
                            let completed =
                                ServedParts::completed(&authored, self.default_response);
                            status = completed.status;
                            status_text = completed.status_text;
                            headers = completed.headers;
                            body = completed.body;
                        }
                        // Not a response (#1589), or the command failed: keep the response
                        // unchanged (issue #269) but signal the failure so it isn't a silent
                        // success (issue #323); a 500 under `strictBehaviors`.
                        Err(e) => {
                            warn!("shellTransform command {cmd:?} failed: {e}");
                            if self.strict {
                                return BehaviorOutcome::StrictFailure(
                                    build_response_with_headers(
                                        StatusCode::INTERNAL_SERVER_ERROR,
                                        [
                                            ("x-rift-imposter", "true"),
                                            ("x-rift-shelltransform-error", "true"),
                                            ("content-type", "application/json"),
                                        ],
                                        crate::response::error_body_typed(
                                            StatusCode::INTERNAL_SERVER_ERROR,
                                            crate::response::ErrorKind::BehaviorError,
                                            &format!(
                                                "shellTransform failed (strictBehaviors): {e}"
                                            ),
                                        ),
                                    ),
                                );
                            }
                            degraded = true;
                            headers.insert(
                                "x-rift-shelltransform-error".to_string(),
                                vec![Spliced::authored("true".to_string())],
                            );
                        }
                    }
                }
            }
        }

        BehaviorOutcome::Applied {
            parts: ServedParts {
                status,
                status_text,
                headers,
                body,
            },
            degraded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use serde_json::json;

    const STUB: StubRef<'static> = StubRef {
        port: 4545,
        index: 0,
        id: None,
    };

    fn behaviors(block: serde_json::Value) -> BehaviorProgram {
        BehaviorProgram::from_elements([&block]).expect("fixture block parses")
    }

    fn original() -> ServedParts {
        ServedParts {
            status: 201,
            status_text: None,
            headers: crate::behaviors::authored_headers(HashMap::from([(
                "set-cookie".to_string(),
                vec!["a=1".to_string(), "b=2".to_string()],
            )])),
            body: Spliced::authored("orig".to_string()),
        }
    }

    async fn run(block: serde_json::Value, strict: bool) -> BehaviorOutcome {
        run_with_default(block, strict, None).await
    }

    async fn run_with_default(
        block: serde_json::Value,
        strict: bool,
        default_response: Option<&IsResponse>,
    ) -> BehaviorOutcome {
        let behaviors = behaviors(block);
        let uri: hyper::Uri = "/users/42?x=1".parse().expect("fixture uri");
        let request_headers: HashMap<String, Vec<String>> = HashMap::new();
        let csv_cache = CsvCache::new();
        BehaviorRun {
            behaviors: &behaviors,
            method: "GET",
            uri: &uri,
            request_headers: &request_headers,
            request_body: None,
            client_addr: None,
            script_state_key: 4545,
            script_files: std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
            stub: STUB,
            csv_cache: &csv_cache,
            script_timeout: Duration::from_secs(5),
            strict,
            default_response,
            shutdown: None,
        }
        .apply(original())
        .await
    }

    /// Run `block` with the imposter teardown signal `shutdown` wired in.
    async fn run_with_shutdown(
        block: serde_json::Value,
        shutdown: &tokio::sync::broadcast::Sender<()>,
    ) -> BehaviorOutcome {
        let behaviors = behaviors(block);
        let uri: hyper::Uri = "/".parse().expect("fixture uri");
        let request_headers: HashMap<String, Vec<String>> = HashMap::new();
        let csv_cache = CsvCache::new();
        BehaviorRun {
            behaviors: &behaviors,
            method: "GET",
            uri: &uri,
            request_headers: &request_headers,
            request_body: None,
            client_addr: None,
            script_state_key: 4545,
            script_files: std::sync::Arc::new(crate::imposter::ScriptFileAccess::Refused),
            stub: STUB,
            csv_cache: &csv_cache,
            script_timeout: Duration::from_secs(5),
            strict: false,
            default_response: None,
            shutdown: Some(shutdown),
        }
        .apply(original())
        .await
    }

    // Issue #1616: a teardown signal during a `wait` ends the pipeline with `Aborted`, and the
    // response for it is the `Empty` carrier the listener turns into a closed connection. Paused
    // time: the hour-long wait is never slept.
    #[tokio::test(start_paused = true)]
    async fn a_teardown_during_a_wait_aborts_the_response() {
        let (shutdown, _keep) = tokio::sync::broadcast::channel(1);
        let started = tokio::time::Instant::now();
        // The step subscribes when it starts sleeping; the teardown comes 10 ms later.
        let teardown = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            shutdown.send(()).expect("the wait is subscribed");
            std::future::pending::<()>().await;
        };
        let outcome = tokio::select! {
            outcome = run_with_shutdown(json!({ "wait": 3_600_000 }), &shutdown) => outcome,
            () = teardown => unreachable!("the teardown future never completes"),
        };
        assert!(matches!(outcome, BehaviorOutcome::Aborted), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "aborted at the teardown, not after the wait: {:?}",
            started.elapsed()
        );
        assert_eq!(
            crate::imposter::fault_io::tcp_fault_carrier(&aborted_response()),
            Some("EMPTY_RESPONSE")
        );
    }

    // Issue #1616: with no teardown, a wait still runs to its end and the response is served.
    #[tokio::test(start_paused = true)]
    async fn a_wait_without_a_teardown_is_served() {
        let (shutdown, _keep) = tokio::sync::broadcast::channel(1);
        let outcome = run_with_shutdown(json!({ "wait": 3_600_000 }), &shutdown).await;
        assert!(
            matches!(
                outcome,
                BehaviorOutcome::Applied {
                    degraded: false,
                    ..
                }
            ),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_successful_transform_is_applied_and_not_degraded() {
        let command = r#"printf '{"statusCode":201,"body":"transformed"}' #"#;
        match run(json!({ "shellTransform": [command] }), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(!degraded);
                assert_eq!(parts.body, "transformed");
                assert_eq!(parts.status, 201);
                // The printed response replaces the original, headers included (#1589).
                assert!(parts.headers.is_empty(), "{:?}", parts.headers);
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("nothing failed")
            }
        }
    }

    #[tokio::test]
    async fn a_lenient_failure_serves_the_original_marked_and_degraded() {
        match run(json!({ "shellTransform": ["exit 3"] }), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(degraded, "a failed behavior must be reported as degraded");
                assert_eq!(parts.body, "orig");
                assert_eq!(parts.status, 201);
                assert_eq!(parts.headers["x-rift-shelltransform-error"], vec!["true"]);
                assert_eq!(parts.headers["set-cookie"], vec!["a=1", "b=2"]);
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("lenient mode must not fail the response")
            }
        }
    }

    #[tokio::test]
    async fn a_strict_failure_is_a_500_carrying_the_signal_header() {
        match run(json!({ "shellTransform": ["exit 3"] }), true).await {
            BehaviorOutcome::StrictFailure(response) => {
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(response.headers()["x-rift-shelltransform-error"], "true");
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .expect("body")
                    .to_bytes();
                let text = String::from_utf8_lossy(&body);
                assert!(
                    text.contains("shellTransform failed (strictBehaviors)"),
                    "{text}"
                );
            }
            BehaviorOutcome::Applied { .. } | BehaviorOutcome::Aborted => {
                panic!("strict mode must fail the response")
            }
        }
    }

    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn a_failed_decorate_is_degraded_and_keeps_the_multi_value_headers() {
        let block =
            json!({ "decorate": "function (request, response) { throw new Error('boom'); }" });
        match run(block.clone(), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(degraded, "a failed decorate must be reported as degraded");
                assert_eq!(parts.body, "orig");
                assert_eq!(parts.status, 201);
                assert_eq!(parts.headers["x-rift-decorate-error"], vec!["true"]);
                assert_eq!(parts.headers["set-cookie"], vec!["a=1", "b=2"]);
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("lenient mode must not fail the response")
            }
        }
        match run(block, true).await {
            BehaviorOutcome::StrictFailure(response) => {
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(response.headers()["x-rift-decorate-error"], "true");
            }
            BehaviorOutcome::Applied { .. } | BehaviorOutcome::Aborted => {
                panic!("strict mode must fail the response")
            }
        }
    }

    #[tokio::test]
    async fn a_failure_stops_the_pipeline_only_in_strict_mode() {
        // Two commands: the first fails, the second would rewrite the body. Lenient keeps going.
        let block = json!({ "shellTransform": ["exit 3", r#"printf '{"body":"second"}' #"#] });
        match run(block.clone(), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(degraded);
                assert_eq!(parts.body, "second");
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("lenient mode must not fail")
            }
        }
        assert!(matches!(
            run(block, true).await,
            BehaviorOutcome::StrictFailure(_)
        ));
    }

    fn applied(outcome: BehaviorOutcome) -> (ServedParts, bool) {
        match outcome {
            BehaviorOutcome::Applied { parts, degraded } => (parts, degraded),
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("nothing should fail")
            }
        }
    }

    /// Issue #1589: the command's stdout is a JSON response that replaces the response, Set-Cookie
    /// included, and what it leaves out comes from `defaultResponse`, as for a returned decorate.
    #[tokio::test]
    async fn shell_stdout_replaces_the_response_completed_from_the_default_response() {
        let default: IsResponse = serde_json::from_value(json!({
            "statusCode": 418, "headers": { "X-Def": "d" }, "body": "dflt"
        }))
        .expect("default response");
        let block = json!({ "shellTransform": r#"printf '{"body":"b"}' #"# });
        let (parts, degraded) =
            applied(run_with_default(block.clone(), false, Some(&default)).await);
        assert!(!degraded);
        assert_eq!(parts.status, 418);
        assert_eq!(parts.body, "b");
        assert_eq!(parts.headers.len(), 1, "{:?}", parts.headers);
        assert_eq!(parts.headers["X-Def"], vec!["d"]);
        // Without a default: the built-in 200 and no headers.
        let (parts, _) = applied(run(block, false).await);
        assert_eq!((parts.status, parts.body.as_str()), (200, "b"));
        assert!(parts.headers.is_empty(), "{:?}", parts.headers);
        // Every field written: status, the whole header map and a non-string body as `is` renders it.
        let block = json!({ "shellTransform":
            r#"printf '{"statusCode":202,"headers":{"X-S":["1","2"]},"body":{"k":3}}' #"# });
        let (parts, _) = applied(run_with_default(block, false, Some(&default)).await);
        assert_eq!(parts.status, 202);
        assert_eq!(parts.headers.len(), 1, "{:?}", parts.headers);
        assert_eq!(parts.headers["X-S"], vec!["1", "2"]);
        assert_eq!(parts.body, r#"{"k":3}"#);
    }

    /// Issue #1589: the command line carries the request and the response JSON as two arguments,
    /// as Mountebank appends them, and `MB_RESPONSE` holds the same response, headers included.
    #[tokio::test]
    async fn the_command_receives_the_request_and_response_as_two_arguments() {
        let block = json!({ "shellTransform":
            r#"f() { printf '{"body":[%s,%s,%s]}' "$1" "$2" "$MB_RESPONSE"; }; f"# });
        let (parts, degraded) = applied(run(block, false).await);
        assert!(!degraded, "{:?}", parts.headers);
        let [request, response, env]: [serde_json::Value; 3] =
            serde_json::from_str(parts.body.as_str()).expect("three JSON values");
        assert_eq!(request["method"], "GET");
        assert_eq!(request["path"], "/users/42");
        assert_eq!(request["query"], json!({ "x": "1" }));
        assert_eq!(
            response,
            json!({ "statusCode": 201, "headers": { "set-cookie": ["a=1", "b=2"] }, "body": "orig" })
        );
        assert_eq!(env, response, "MB_RESPONSE is the second argument");
    }

    /// Issue #1589: each step reads the response the previous one returned, with its body's JSON
    /// type, so `body += 1` on a number adds (Mountebank's "array of shell transforms in order").
    #[tokio::test]
    async fn a_chained_command_sees_the_previous_commands_response() {
        let block = json!({ "shellTransform": [
            r#"printf '{"statusCode":203,"body":2}' #"#,
            r#"printf '{"body":%s}' "$MB_RESPONSE" #"#,
        ] });
        let (parts, _) = applied(run(block, false).await);
        let seen: serde_json::Value = serde_json::from_str(parts.body.as_str()).expect("JSON body");
        assert_eq!(seen, json!({ "statusCode": 203, "headers": {}, "body": 2 }));
    }

    /// Issue #1589: stdout that is valid JSON but not an object replaces the response with an empty
    /// one, as Mountebank reads no field of it.
    #[tokio::test]
    async fn a_non_object_stdout_is_an_empty_response() {
        for output in ["3", r#""text""#, "[1]"] {
            let block = json!({ "shellTransform": format!("printf '{output}' #") });
            let (parts, degraded) = applied(run(block, false).await);
            assert!(!degraded, "{output}");
            assert_eq!((parts.status, parts.body.as_str()), (200, ""), "{output}");
            assert!(parts.headers.is_empty(), "{output}: {:?}", parts.headers);
        }
    }

    /// Issue #1589: stdout that is not a JSON response — plain text, empty, `null` — is a failed
    /// shellTransform, never a body: the original is kept and marked, or a 500 under strict.
    #[tokio::test]
    async fn stdout_that_is_not_a_json_response_fails_the_transform() {
        for (command, message) in [
            ("echo hello", "Shell command returned invalid JSON"),
            ("true", "Shell command returned invalid JSON"),
            ("printf null #", "Shell command returned null"),
        ] {
            let block = json!({ "shellTransform": command });
            let (parts, degraded) = applied(run(block.clone(), false).await);
            assert!(degraded, "{command}");
            assert_eq!(
                (parts.status, parts.body.as_str()),
                (201, "orig"),
                "{command}"
            );
            assert_eq!(parts.headers["x-rift-shelltransform-error"], vec!["true"]);
            assert_eq!(parts.headers["set-cookie"], vec!["a=1", "b=2"], "{command}");
            match run(block, true).await {
                BehaviorOutcome::StrictFailure(response) => {
                    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                    let body = response
                        .into_body()
                        .collect()
                        .await
                        .expect("body")
                        .to_bytes();
                    let text = String::from_utf8_lossy(&body);
                    assert!(text.contains(message), "{command}: {text}");
                }
                BehaviorOutcome::Applied { .. } | BehaviorOutcome::Aborted => {
                    panic!("strict mode must fail: {command}")
                }
            }
        }
    }

    // Issue #1567: a decorate's returned object replaces the response, Set-Cookie included, and
    // what it leaves out comes from `defaultResponse`, as in Mountebank.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn a_returned_decorate_object_is_completed_from_the_default_response() {
        let default: IsResponse = serde_json::from_value(json!({
            "statusCode": 418, "headers": { "X-Def": "d" }, "body": "dflt"
        }))
        .expect("default response");
        let block = json!({ "decorate": "config => ({ body: 'ret' })" });
        match run_with_default(block.clone(), false, Some(&default)).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(!degraded);
                assert_eq!(parts.status, 418);
                assert_eq!(parts.body, "ret");
                assert_eq!(parts.headers.len(), 1, "{:?}", parts.headers);
                assert_eq!(parts.headers["X-Def"], vec!["d"]);
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("nothing failed")
            }
        }
        // Without a default: the built-in 200, no headers.
        match run(block, false).await {
            BehaviorOutcome::Applied { parts, .. } => {
                assert_eq!(parts.status, 200);
                assert_eq!(parts.body, "ret");
                assert!(parts.headers.is_empty(), "{:?}", parts.headers);
            }
            BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                panic!("nothing failed")
            }
        }
    }

    // A mutated response keeps its held-aside Set-Cookie lines; a deleted header is gone.
    // Returning the response it was given (`return response;`, `return config.response;`) is a
    // mutation too, not a replacement.
    #[cfg(feature = "javascript")]
    #[tokio::test]
    async fn a_mutating_decorate_keeps_set_cookie_and_drops_deleted_headers() {
        for script in [
            "(request, response) => { response.headers['X-A'] = '1'; delete response.headers['X-A']; response.body = 'm'; }",
            "function (request, response) { response.headers['X-A'] = '1'; delete response.headers['X-A']; response.body = 'm'; return response; }",
            "config => { config.response.headers['X-A'] = '1'; delete config.response.headers['X-A']; config.response.body = 'm'; return config.response; }",
        ] {
            match run(json!({ "decorate": script }), false).await {
                BehaviorOutcome::Applied { parts, degraded } => {
                    assert!(!degraded, "{script}");
                    assert_eq!((parts.status, parts.body.as_str()), (201, "m"), "{script}");
                    assert!(!parts.headers.contains_key("X-A"), "{script}");
                    assert_eq!(parts.headers["set-cookie"], vec!["a=1", "b=2"], "{script}");
                }
                BehaviorOutcome::StrictFailure(_) | BehaviorOutcome::Aborted => {
                    panic!("nothing failed: {script}")
                }
            }
        }
    }
}
