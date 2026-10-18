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
    BehaviorProgram, BehaviorStep, CsvCache, Decorated, RequestContext, ResponseStatus, Spliced,
    SplicedHeaders, apply_copy_spliced, apply_lookup_spliced, apply_shell_transform,
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
    /// A behavior failed under `strictBehaviors` (#375): serve this error response instead.
    StrictFailure(Response<Full<Bytes>>),
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
    pub stub: StubRef<'a>,
    pub csv_cache: &'a CsvCache,
    pub script_timeout: Duration,
    pub strict: bool,
    /// The imposter's `defaultResponse`: it completes a response a `decorate` returned (issue
    /// #1567), as Mountebank completes every response it serves.
    pub default_response: Option<&'a IsResponse>,
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
                    let wait_ms = wait.get_duration_ms();
                    if wait_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(wait_ms)).await;
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
                // shellTransform (issue #269): pipe the body through external command(s);
                // stdout becomes the new body. Runs independently of copy/lookup/decorate.
                BehaviorStep::ShellTransform(cmd) => {
                    // Run the fork/exec/wait off the tokio worker (issue #478): a synchronous
                    // subprocess run inline would stall the worker for its whole lifetime,
                    // starving unrelated requests multiplexed on it.
                    let shell_result = {
                        let cmd = cmd.clone();
                        let rc = request_context.get_or_init(build_request_context).clone();
                        let body_in = body.as_str().to_string();
                        let status_in = script_status(status, &status_text);
                        // Plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987):
                        // `apply_shell_transform` is a fork/exec of an external command and never
                        // touches a `FlowStore`, so nothing here can annotate.
                        tokio::task::spawn_blocking(move || {
                            apply_shell_transform(&cmd, &rc, &body_in, &status_in)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            Err(std::io::Error::other(format!(
                                "shellTransform task panicked: {e}"
                            )))
                        })
                    };
                    match shell_result {
                        Ok(transformed) => body = Spliced::authored(transformed),
                        // Keep the body unchanged (issue #269) but signal the failure so it
                        // isn't a silent success (issue #323).
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
            stub: STUB,
            csv_cache: &csv_cache,
            script_timeout: Duration::from_secs(5),
            strict,
            default_response,
        }
        .apply(original())
        .await
    }

    #[tokio::test]
    async fn a_successful_transform_is_applied_and_not_degraded() {
        match run(json!({ "shellTransform": ["printf transformed"] }), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(!degraded);
                assert_eq!(parts.body, "transformed");
                assert_eq!(parts.status, 201);
                assert_eq!(parts.headers["set-cookie"], vec!["a=1", "b=2"]);
            }
            BehaviorOutcome::StrictFailure(_) => panic!("nothing failed"),
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
            BehaviorOutcome::StrictFailure(_) => panic!("lenient mode must not fail the response"),
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
            BehaviorOutcome::Applied { .. } => panic!("strict mode must fail the response"),
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
            BehaviorOutcome::StrictFailure(_) => panic!("lenient mode must not fail the response"),
        }
        match run(block, true).await {
            BehaviorOutcome::StrictFailure(response) => {
                assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(response.headers()["x-rift-decorate-error"], "true");
            }
            BehaviorOutcome::Applied { .. } => panic!("strict mode must fail the response"),
        }
    }

    #[tokio::test]
    async fn a_failure_stops_the_pipeline_only_in_strict_mode() {
        // Two commands: the first fails, the second would rewrite the body. Lenient keeps going.
        let block = json!({ "shellTransform": ["exit 3", "printf second"] });
        match run(block.clone(), false).await {
            BehaviorOutcome::Applied { parts, degraded } => {
                assert!(degraded);
                assert_eq!(parts.body, "second");
            }
            BehaviorOutcome::StrictFailure(_) => panic!("lenient mode must not fail"),
        }
        assert!(matches!(
            run(block, true).await,
            BehaviorOutcome::StrictFailure(_)
        ));
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
            BehaviorOutcome::StrictFailure(_) => panic!("nothing failed"),
        }
        // Without a default: the built-in 200, no headers.
        match run(block, false).await {
            BehaviorOutcome::Applied { parts, .. } => {
                assert_eq!(parts.status, 200);
                assert_eq!(parts.body, "ret");
                assert!(parts.headers.is_empty(), "{:?}", parts.headers);
            }
            BehaviorOutcome::StrictFailure(_) => panic!("nothing failed"),
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
                BehaviorOutcome::StrictFailure(_) => panic!("nothing failed: {script}"),
            }
        }
    }
}
