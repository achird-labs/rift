//! Test-only helpers shared across this crate's unit tests.

use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Metadata, Subscriber, span};

/// One captured `rift::template` event, as name → `Debug`-rendered value.
#[derive(Default, Clone)]
pub(crate) struct CapturedEvent {
    pub(crate) fields: Vec<(String, String)>,
}

impl CapturedEvent {
    pub(crate) fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Run `f` with a subscriber that captures every `"rift::template"` event, field by field.
///
/// Hand-written rather than a `tracing-subscriber` layer for the reason the sibling capture in
/// `scripting/trace.rs` gives: `rift-mock-core` does not otherwise depend on
/// `tracing-subscriber`, and this is a handful of trait methods. `tracing_test::traced_test`
/// is not an option either — it installs an `EnvFilter` of `"<crate_name>=trace"`
/// (tracing-test-macro-0.2.5/src/lib.rs:73-78), which drops an event raised on a
/// `rift::template` target, so a `logs_contain` assertion there would pass against an
/// implementation that logs nothing at all.
pub(crate) fn captured_logs(f: impl FnOnce()) -> Vec<CapturedEvent> {
    // Without it a callsite first reached on another thread hides this capture's events (#1447).
    crate::scripting::keep_scoped_captures_visible();
    let events = Arc::new(Mutex::new(Vec::new()));
    let capture = TemplateLogCapture {
        events: Arc::clone(&events),
    };
    tracing::subscriber::with_default(capture, f);
    let collected = events.lock().expect("capture buffer");
    collected.clone()
}

struct TemplateLogCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl Subscriber for TemplateLogCapture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "rift::template"
    }

    fn new_span(&self, _span: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }

    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut captured = CapturedEvent::default();
        event.record(&mut captured);
        self.events.lock().expect("capture buffer").push(captured);
    }

    fn enter(&self, _span: &span::Id) {}

    fn exit(&self, _span: &span::Id) {}
}

impl Visit for CapturedEvent {
    /// `&str` fields arrive here, not through `record_debug`. Overriding it is what lets a test
    /// assert the bare value (`checkout`) rather than its quoted `Debug` form — and, more to the
    /// point, lets the production code record a `&str` field plainly, the way every other string
    /// field in the crate does, instead of reaching for `%` to dodge the quotes.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_warning(value: &str) {
        tracing::warn!(target: "rift::template", value, "probe");
    }

    /// Issue #1447: another thread reaching the callsite first, while this capture is the only
    /// registered dispatcher, cached `Interest::never` and hid the capturing thread's own event.
    /// The dispatcher count is process-wide, so this is red without the fix only when run alone
    /// (`--exact`); `tests/issue_1447_scoped_capture.rs` pins it in a process of its own.
    #[test]
    fn a_callsite_another_thread_registered_first_is_still_captured() {
        let events = captured_logs(|| {
            std::thread::spawn(|| template_warning("elsewhere"))
                .join()
                .expect("thread");
            template_warning("here");
        });
        assert_eq!(events.len(), 1, "only the capturing thread's event");
        assert_eq!(events[0].field("value"), Some("here"));
    }
}
