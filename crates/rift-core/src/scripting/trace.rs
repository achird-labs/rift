//! Script decision/log tracing (issue #360 Items 2/3): capturing `ctx.logger` output for `rift
//! script run`, and the debug-mode per-request script trace surfaced on `x-rift-script-trace`.
//!
//! `ctx.logger` (issue #355 P1) routes to `tracing` at a fixed target, `"rift::script"`. Outside
//! a running server there is no global `tracing` subscriber installed (`rift script run`/`check`
//! are one-shot CLI invocations), and even inside a running server the debug-mode trace needs to
//! capture only ONE request's log lines, not everything any subscriber sees process-wide. Both
//! are solved the same way: [`capture_script_logs`] installs a subscriber scoped to a single
//! closure call via `tracing::subscriber::with_default` (a thread-local override, not a global
//! one), so it composes with whatever subscriber (if any) is already installed.
//!
//! This is a hand-written [`tracing::Subscriber`] rather than a `tracing-subscriber` `Layer` —
//! `rift-core` doesn't otherwise depend on `tracing-subscriber`, and capturing one field from one
//! target is a handful of trait methods, not worth a new dependency.

use super::FaultDecision;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

/// The `tracing` target every `ctx.logger` call is routed to (issue #355 P1).
const SCRIPT_LOG_TARGET: &str = "rift::script";

/// Max `ctx.logger` lines kept in a debug-mode [`ScriptTraceEntry`] (issue #360): the trace ships
/// on the `x-rift-script-trace` response header, so a chatty script must not blow the header up
/// unbounded. `rift script run` output is uncapped (it's a terminal dump, not a header) — only
/// the header-bound trace applies this via [`cap_trace_logs`].
const MAX_TRACE_LOG_LINES: usize = 50;

/// Max characters per retained trace log line (each is also body-capped like the decision), so one
/// enormous `ctx.logger` line can't dominate the header either.
const MAX_TRACE_LOG_LINE_CHARS: usize = 500;

/// Cap `logs` for a header-bound trace entry: at most [`MAX_TRACE_LOG_LINES`] lines (with an
/// elision marker naming how many were dropped), each truncated to [`MAX_TRACE_LOG_LINE_CHARS`].
pub fn cap_trace_logs(mut logs: Vec<String>) -> Vec<String> {
    let dropped = logs.len().saturating_sub(MAX_TRACE_LOG_LINES);
    logs.truncate(MAX_TRACE_LOG_LINES);
    for line in &mut logs {
        if line.chars().count() > MAX_TRACE_LOG_LINE_CHARS {
            let truncated: String = line.chars().take(MAX_TRACE_LOG_LINE_CHARS).collect();
            *line = format!("{truncated}…");
        }
    }
    if dropped > 0 {
        logs.push(format!("… ({dropped} more log line(s) elided)"));
    }
    logs
}

/// One script hook invocation's trace record: which hook ran, its rendered decision, how long it
/// took, and any `ctx.logger` lines it emitted. `cache` is `Some("hit"|"miss")` only on the
/// proxy path's `DecisionCache`-backed hook; `None` elsewhere (e.g. every imposter-stub hook,
/// which has no decision cache).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScriptTraceEntry {
    pub hook: String,
    pub decision: String,
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
    pub logs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
}

/// Render a [`FaultDecision`] the way `rift script run`/the debug trace print it: `pass()` /
/// `delay(<ms>ms)` / `reset()` / `http(<status>) { headers } body=<preview>`.
pub fn render_decision(decision: &FaultDecision) -> String {
    match decision {
        FaultDecision::None => "pass()".to_string(),
        FaultDecision::Latency { duration_ms, .. } => format!("delay({duration_ms}ms)"),
        FaultDecision::Reset { .. } => "reset()".to_string(),
        FaultDecision::Error {
            status,
            body,
            headers,
            ..
        } => {
            let mut rendered = format!("http({status})");
            if !headers.is_empty() {
                let mut pairs: Vec<String> = headers
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {v:?}"))
                    .collect();
                pairs.sort();
                rendered.push_str(&format!(" {{ {} }}", pairs.join(", ")));
            }
            if !body.is_empty() {
                // Cap the preview so a large response body doesn't blow up a header/CLI line.
                let preview: String = body.chars().take(200).collect();
                let truncated = preview.len() < body.len();
                rendered.push_str(&format!(
                    " body={preview:?}{}",
                    if truncated { "…" } else { "" }
                ));
            }
            rendered
        }
    }
}

/// Run `f` with a subscriber that captures every `"rift::script"` event into an in-memory
/// buffer, returning `f`'s result alongside the captured log lines in call order.
/// Thread-scoped: doesn't touch or require any process-wide default subscriber.
pub fn capture_script_logs<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let capture = ScriptLogCapture {
        logs: Arc::clone(&logs),
    };
    let result = tracing::subscriber::with_default(capture, f);
    let collected = logs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    (result, collected)
}

/// A minimal [`Subscriber`] that only records events at [`SCRIPT_LOG_TARGET`]'s "message" field.
/// Spans are accepted but not tracked (`ctx.logger` never opens one) — `new_span` always returns
/// the same id, matching the trivial no-op span handling this capture needs.
struct ScriptLogCapture {
    logs: Arc<Mutex<Vec<String>>>,
}

impl Subscriber for ScriptLogCapture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == SCRIPT_LOG_TARGET
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            self.logs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message);
        }
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

/// Extracts the `message` field's text from a `tracing` event.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn render_decision_variants() {
        assert_eq!(render_decision(&FaultDecision::None), "pass()");
        assert_eq!(
            render_decision(&FaultDecision::Latency {
                duration_ms: 42,
                rule_id: "r".into(),
            }),
            "delay(42ms)"
        );
        assert_eq!(
            render_decision(&FaultDecision::Reset {
                rule_id: "r".into()
            }),
            "reset()"
        );
        let mut headers = HashMap::new();
        headers.insert("Retry-After".to_string(), "1".to_string());
        let rendered = render_decision(&FaultDecision::Error {
            status: 503,
            body: "boom".to_string(),
            rule_id: "r".into(),
            headers,
        });
        assert!(rendered.starts_with("http(503)"), "got {rendered}");
        assert!(
            rendered.contains("\"Retry-After\": \"1\""),
            "got {rendered}"
        );
        assert!(rendered.contains("body=\"boom\""), "got {rendered}");
    }

    // Issue #360 Item 2: `ctx.logger` lines emitted during `f` are captured, in order, and
    // events at other targets are ignored.
    #[test]
    fn capture_script_logs_collects_only_the_script_target() {
        let (value, logs) = capture_script_logs(|| {
            tracing::info!(target: "rift::script", "first");
            tracing::warn!(target: "other::target", "ignored");
            tracing::error!(target: "rift::script", "second");
            42
        });
        assert_eq!(value, 42);
        assert_eq!(logs, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn capture_script_logs_empty_when_nothing_logged() {
        let (_, logs) = capture_script_logs(|| ());
        assert!(logs.is_empty());
    }

    // Issue #360: the header-bound trace caps a chatty script's logs (line count + per-line
    // length) so the `x-rift-script-trace` header can't grow unbounded.
    #[test]
    fn cap_trace_logs_bounds_line_count_and_length() {
        let logs: Vec<String> = (0..MAX_TRACE_LOG_LINES + 20)
            .map(|i| format!("line {i}"))
            .collect();
        let capped = cap_trace_logs(logs);
        assert_eq!(capped.len(), MAX_TRACE_LOG_LINES + 1); // +1 for the elision marker
        assert!(
            capped
                .last()
                .unwrap()
                .contains("20 more log line(s) elided")
        );

        let long = vec!["x".repeat(MAX_TRACE_LOG_LINE_CHARS + 100)];
        let capped = cap_trace_logs(long);
        assert_eq!(capped.len(), 1);
        assert!(capped[0].ends_with('…'));
        assert_eq!(capped[0].chars().count(), MAX_TRACE_LOG_LINE_CHARS + 1);
    }

    #[test]
    fn cap_trace_logs_leaves_small_logs_untouched() {
        let logs = vec!["a".to_string(), "b".to_string()];
        assert_eq!(cap_trace_logs(logs.clone()), logs);
    }
}
