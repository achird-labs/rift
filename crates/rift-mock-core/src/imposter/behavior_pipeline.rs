//! The response behavior pipeline (`_behaviors`): wait → copy → lookup → decorate →
//! shellTransform, applied to a response's status, headers and body.
//!
//! One function so every response type that carries behaviors runs the same pipeline with the
//! same failure contract (issue #1184). The `is` and `inject` serve paths call it.

use super::handler::SCRIPT_TIMEOUT_HEADER;
use super::headers::StubRef;
use super::response::apply_decorate_bounded;
use crate::behaviors::{
    CsvCache, RequestContext, ResponseBehaviors, apply_copy_behaviors, apply_lookup_behaviors,
    apply_shell_transform,
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
#[derive(Debug)]
pub(crate) struct ServedParts {
    pub status: u16,
    /// Multi-value (issue #238): one entry per header name, one element per header line.
    pub headers: HashMap<String, Vec<String>>,
    pub body: String,
}

/// What running the pipeline produced. A dropped `StrictFailure` would serve the response the
/// strict contract refused, so the outcome must be matched.
#[derive(Debug)]
#[must_use]
pub(crate) enum BehaviorOutcome {
    /// The response to serve. `degraded` is true when a behavior failed under the lenient
    /// contract (#269/#323): the response is served, but carries a signal header and is not the
    /// response the configuration asked for.
    Applied {
        parts: ServedParts,
        #[cfg_attr(
            not(test),
            expect(
                dead_code,
                reason = "the proxy path reads it to skip recording (#1189)"
            )
        )]
        degraded: bool,
    },
    /// A behavior failed under `strictBehaviors` (#375): serve this error response instead.
    StrictFailure(Response<Full<Bytes>>),
}

/// Everything the pipeline reads besides the response itself.
pub(crate) struct BehaviorRun<'a, SH> {
    pub behaviors: &'a ResponseBehaviors,
    pub method: &'a str,
    pub uri: &'a hyper::Uri,
    pub request_headers: &'a HashMap<String, Vec<String>, SH>,
    pub request_body: Option<&'a str>,
    pub script_state_key: u16,
    pub stub: StubRef<'a>,
    pub csv_cache: &'a CsvCache,
    pub script_timeout: Duration,
    pub strict: bool,
}

impl<SH: BuildHasher> BehaviorRun<'_, SH> {
    /// Run the pipeline over `parts`, in Mountebank's order.
    pub(crate) async fn apply(&self, parts: ServedParts) -> BehaviorOutcome {
        let ServedParts {
            mut status,
            mut headers,
            mut body,
        } = parts;
        let behaviors = self.behaviors;
        let mut degraded = false;

        if let Some(ref wait) = behaviors.wait {
            let wait_ms = wait.get_duration_ms();
            if wait_ms > 0 {
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
            }
        }

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
            )
        };

        // copy/lookup are pure token substitution — apply them across each value of
        // multi-value headers so multiplicity survives (e.g. multiple Set-Cookie;
        // RFC 7230 §3.2.2 forbids folding Set-Cookie). decorate uses a single-value
        // JS/Rhai object model, so only that path collapses — and even there Set-Cookie
        // is held aside, never comma-folded.
        if !behaviors.copy.is_empty() {
            body = apply_copy_behaviors(
                &body,
                &mut headers,
                &behaviors.copy,
                request_context.get_or_init(build_request_context),
                self.stub,
            );
        }
        if !behaviors.lookup.is_empty() {
            body = apply_lookup_behaviors(
                &body,
                &mut headers,
                &behaviors.lookup,
                request_context.get_or_init(build_request_context),
                self.csv_cache,
                self.stub,
            );
        }
        if let Some(ref decorate_script) = behaviors.decorate {
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

            let set_cookie: Vec<(String, Vec<String>)> = headers
                .iter()
                .filter(|(k, _)| is_set_cookie(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let single: HashMap<String, String> = headers
                .iter()
                .filter(|(k, _)| !is_set_cookie(k))
                .map(|(k, v)| (k.clone(), v.join(", ")))
                .collect();
            match apply_decorate_bounded(
                decorate_script.clone(),
                request_context.get_or_init(build_request_context).clone(),
                body.clone(),
                status,
                single,
                self.script_state_key,
                self.stub.id.map(str::to_owned),
                self.script_timeout,
            )
            .await
            {
                Ok((new_body, new_status, single)) => {
                    body = new_body;
                    status = new_status;
                    // Restore the held-aside Set-Cookie lines unless the script set its
                    // own (case-insensitively) — a script override wins deterministically.
                    let script_set_cookie = single.keys().any(|k| is_set_cookie(k));
                    headers = single.into_iter().map(|(k, v)| (k, vec![v])).collect();
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
                    let timed_out = matches!(e, crate::behaviors::DecorateError::Timeout(_));
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
                        return BehaviorOutcome::StrictFailure(build_response_with_headers(
                            status,
                            hdrs,
                            crate::response::error_body_typed(
                                status,
                                crate::response::ErrorKind::BehaviorError,
                                &format!("decorate failed (strictBehaviors): {e}"),
                            ),
                        ));
                    }
                    degraded = true;
                    headers.insert(
                        "x-rift-decorate-error".to_string(),
                        vec!["true".to_string()],
                    );
                    if timed_out {
                        headers.insert(SCRIPT_TIMEOUT_HEADER.to_string(), vec!["true".to_string()]);
                    }
                }
            }
        }

        // shellTransform (issue #269): pipe the body through external command(s);
        // stdout becomes the new body. Runs independently of copy/lookup/decorate.
        for cmd in &behaviors.shell_transform {
            // Run the fork/exec/wait off the tokio worker (issue #478): a synchronous
            // subprocess run inline would stall the worker for its whole lifetime,
            // starving unrelated requests multiplexed on it.
            let shell_result = {
                let cmd = cmd.clone();
                let rc = request_context.get_or_init(build_request_context).clone();
                let body_in = body.clone();
                // Plain `spawn_blocking`, not `spawn_blocking_annotated` (issue #987):
                // `apply_shell_transform` is a fork/exec of an external command and never
                // touches a `FlowStore`, so nothing here can annotate.
                tokio::task::spawn_blocking(move || {
                    apply_shell_transform(&cmd, &rc, &body_in, status)
                })
                .await
                .unwrap_or_else(|e| {
                    Err(std::io::Error::other(format!(
                        "shellTransform task panicked: {e}"
                    )))
                })
            };
            match shell_result {
                Ok(transformed) => body = transformed,
                // Keep the body unchanged (issue #269) but signal the failure so it
                // isn't a silent success (issue #323).
                Err(e) => {
                    warn!("shellTransform command {cmd:?} failed: {e}");
                    if self.strict {
                        return BehaviorOutcome::StrictFailure(build_response_with_headers(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            [
                                ("x-rift-imposter", "true"),
                                ("x-rift-shelltransform-error", "true"),
                                ("content-type", "application/json"),
                            ],
                            crate::response::error_body_typed(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                crate::response::ErrorKind::BehaviorError,
                                &format!("shellTransform failed (strictBehaviors): {e}"),
                            ),
                        ));
                    }
                    degraded = true;
                    headers.insert(
                        "x-rift-shelltransform-error".to_string(),
                        vec!["true".to_string()],
                    );
                }
            }
        }

        BehaviorOutcome::Applied {
            parts: ServedParts {
                status,
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

    fn behaviors(block: serde_json::Value) -> ResponseBehaviors {
        serde_json::from_value(block).expect("fixture block parses")
    }

    fn original() -> ServedParts {
        ServedParts {
            status: 201,
            headers: HashMap::from([(
                "set-cookie".to_string(),
                vec!["a=1".to_string(), "b=2".to_string()],
            )]),
            body: "orig".to_string(),
        }
    }

    async fn run(block: serde_json::Value, strict: bool) -> BehaviorOutcome {
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
            script_state_key: 4545,
            stub: STUB,
            csv_cache: &csv_cache,
            script_timeout: Duration::from_secs(5),
            strict,
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
}
