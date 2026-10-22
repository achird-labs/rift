//! The in-memory log buffer behind `GET /logs` (issue #1615).
//!
//! Mountebank's `/logs` re-reads its whole JSON logfile on every request, and that file never
//! rotates (its `maxsize: '20m'` is a string, so winston's `size >= maxsize` is always false). Its
//! "no cap" is an unbounded read. Rift keeps the most recent entries in a ring instead, so the
//! memory a long-lived process spends on it is a constant: at most [`MAX_ENTRIES`] entries and
//! [`MAX_BYTES`] bytes, whichever bites first.
//!
//! The ring is a `tracing` [`Layer`]. Install it as a sibling of the console layer under the
//! global `EnvFilter` and it sees exactly the events `--loglevel` / `RUST_LOG` let through.
//!
//! Events whose target is [`EXCLUDED_TARGET`] are never recorded. A script's own `logger.*` /
//! `console.*` output is authored by whoever wrote the script and routinely echoes request data
//! (tokens, cookies), and `GET /logs` is readable by anyone holding the admin key, which is a
//! wider audience than the process's stderr. The same applies to anything a future change keeps
//! out of the logs on purpose: the ring records what the subscriber hands it, nothing more.

use serde::Serialize;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Most entries retained; the oldest is evicted past this.
pub const MAX_ENTRIES: usize = 10_000;
/// Most bytes retained (message text plus [`ENTRY_OVERHEAD`] per entry).
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Longest message kept, in bytes; a longer one is cut on a char boundary and ends in `…`.
pub const MAX_MESSAGE: usize = 4 * 1024;
/// Bytes charged per entry on top of its message (level, timestamp, bookkeeping).
const ENTRY_OVERHEAD: usize = 64;
/// Target of script-authored log output, which the ring does not record (see the module docs).
pub const EXCLUDED_TARGET: &str = "rift::script";

/// One `GET /logs` entry, in Mountebank's shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    pub level: &'static str,
    pub message: String,
    pub timestamp: String,
}

impl LogEntry {
    fn cost(&self) -> usize {
        self.message.len() + ENTRY_OVERHEAD
    }
}

#[derive(Debug, Default)]
struct Inner {
    entries: VecDeque<LogEntry>,
    /// Absolute index of `entries[0]` (entries logged since start that were already evicted).
    first_index: u64,
    bytes: usize,
}

impl Inner {
    fn next_index(&self) -> u64 {
        self.first_index + self.entries.len() as u64
    }
}

/// A bounded, shareable log buffer; clone it to hand the same buffer to the layer and the server.
#[derive(Debug, Clone, Default)]
pub struct LogRing(Arc<Mutex<Inner>>);

impl LogRing {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Poison recovery, not a swallow: the guarded state is only ever mutated by `push`, which
        // cannot leave it half-updated in a way that breaks the bounds (a panic there is before
        // any mutation), so continuing with the inner value is sound.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Append an entry, evicting from the front until both bounds hold.
    pub fn push(&self, mut entry: LogEntry) {
        truncate_on_char_boundary(&mut entry.message, MAX_MESSAGE);
        let mut inner = self.lock();
        inner.bytes += entry.cost();
        inner.entries.push_back(entry);
        while inner.entries.len() > MAX_ENTRIES || inner.bytes > MAX_BYTES {
            let Some(old) = inner.entries.pop_front() else {
                break;
            };
            inner.bytes -= old.cost();
            inner.first_index += 1;
        }
    }

    /// Number of entries currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The entries Mountebank's `/logs` would return for these raw query values: JavaScript
    /// `allLogs.slice(startIndex, endIndex + 1)` over the virtual array of every entry logged
    /// since start, with `startIndex` defaulting to 0 and `endIndex` to the last entry, both read
    /// with `parseInt`. Entries already evicted are simply absent from the result.
    #[must_use]
    pub fn slice(&self, start: Option<&str>, end: Option<&str>) -> Vec<LogEntry> {
        let inner = self.lock();
        let len = i64::try_from(inner.next_index()).unwrap_or(i64::MAX);
        // `||` in mb: an empty value counts as absent.
        let start = start.filter(|s| !s.is_empty());
        let end = end.filter(|s| !s.is_empty());
        // NaN `startIndex` slices from 0; NaN `endIndex` (NaN + 1) slices to 0, i.e. nothing.
        let start = start.map_or(0, |s| parse_int(s).unwrap_or(0));
        let end = end.map_or(len, |e| parse_int(e).map_or(0, |v| v.saturating_add(1)));
        let (from, to) = (js_clamp(start, len), js_clamp(end, len));
        if from >= to {
            return Vec::new();
        }
        // Both are in 0..=len, so these conversions cannot fail.
        let (from, to) = (from as u64, to as u64);
        let first = inner.first_index;
        let lo = from.max(first);
        if lo >= to {
            return Vec::new();
        }
        inner
            .entries
            .iter()
            .skip((lo - first) as usize)
            .take((to - lo) as usize)
            .cloned()
            .collect()
    }
}

/// `Array.prototype.slice`'s relative-index clamp.
fn js_clamp(index: i64, len: i64) -> i64 {
    if index < 0 {
        len.saturating_add(index).max(0)
    } else {
        index.min(len)
    }
}

/// JavaScript `parseInt(s, 10)`: leading whitespace, an optional sign, the leading digits; `None`
/// is NaN. Out-of-range values saturate, which is the same slice once clamped.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (negative, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let magnitude = rest[..digits].bytes().fold(0i64, |acc, d| {
        acc.saturating_mul(10).saturating_add(i64::from(d - b'0'))
    });
    Some(if negative { -magnitude } else { magnitude })
}

fn truncate_on_char_boundary(message: &mut String, max: usize) {
    if message.len() <= max {
        return;
    }
    let mut cut = max.saturating_sub('…'.len_utf8());
    while !message.is_char_boundary(cut) {
        cut -= 1;
    }
    message.truncate(cut);
    message.push('…');
}

/// Winston's level names for tracing's.
fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "error",
        Level::WARN => "warn",
        Level::INFO => "info",
        Level::DEBUG | Level::TRACE => "debug",
    }
}

/// Renders the `message` field, then ` key=value` for the others, with no ANSI and no target.
/// Stops growing at [`MAX_MESSAGE`] so a huge field is not copied only to be truncated.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn field(&mut self, field: &Field, value: &dyn std::fmt::Display) {
        if self.fields.len() < MAX_MESSAGE {
            // Writing to a String cannot fail.
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.field(field, &value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // Writing to a String cannot fail.
            let _ = write!(self.message, "{value:?}");
        } else if self.fields.len() < MAX_MESSAGE {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

impl<S: Subscriber> Layer<S> for LogRing {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if meta.target() == EXCLUDED_TARGET {
            return;
        }
        // Formatted before the lock is taken, and emitting nothing: this runs inside the
        // subscriber, so an event here would recurse.
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let mut message = visitor.message;
        message.push_str(&visitor.fields);
        self.push(LogEntry {
            level: level_name(*meta.level()),
            message,
            timestamp: chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            level: "info",
            message: message.to_string(),
            timestamp: String::new(),
        }
    }

    fn ring_of(n: usize) -> LogRing {
        let ring = LogRing::new();
        for i in 0..n {
            ring.push(entry(&i.to_string()));
        }
        ring
    }

    fn messages(entries: &[LogEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.message.as_str()).collect()
    }

    #[test]
    fn slice_follows_javascript_semantics() {
        let ring = ring_of(5);
        let all = ["0", "1", "2", "3", "4"];
        let cases: &[(Option<&str>, Option<&str>, &[&str])] = &[
            (None, None, &all),
            (Some("1"), Some("2"), &["1", "2"]),
            (None, Some("abc"), &[]),
            (Some("abc"), Some("3"), &["0", "1", "2", "3"]),
            (Some("-2"), None, &["3", "4"]),
            (Some("999"), None, &[]),
            (Some("2x"), Some("3"), &["2", "3"]),
            (Some(" 1"), Some("+1"), &["1"]),
            (Some("3"), Some("1"), &[]),
            (Some(""), Some(""), &all),
            (None, Some("-2"), &["0", "1", "2", "3"]),
            (None, Some("99999999999999999999999"), &all),
        ];
        for (start, end, want) in cases {
            assert_eq!(
                messages(&ring.slice(*start, *end)),
                *want,
                "start={start:?} end={end:?}"
            );
        }
    }

    #[test]
    fn eviction_by_count_keeps_absolute_indices() {
        let ring = ring_of(MAX_ENTRIES + 3);
        assert_eq!(ring.len(), MAX_ENTRIES);
        // Entries 0..3 are gone; index 3 is the oldest retained and is still addressed as 3.
        let got = ring.slice(Some("0"), Some("4"));
        assert_eq!(messages(&got), ["3", "4"]);
        let polled = ring.slice(Some(&(MAX_ENTRIES + 2).to_string()), None);
        assert_eq!(messages(&polled), [(MAX_ENTRIES + 2).to_string()]);
    }

    #[test]
    fn eviction_by_bytes_bounds_memory() {
        let ring = LogRing::new();
        let big = "x".repeat(MAX_MESSAGE);
        for _ in 0..2_000 {
            ring.push(entry(&big));
        }
        let inner = ring.lock();
        assert!(inner.bytes <= MAX_BYTES, "{}", inner.bytes);
        assert!(inner.entries.len() < 2_000);
        assert_eq!(inner.next_index(), 2_000);
        assert_eq!(
            inner.bytes,
            inner.entries.iter().map(LogEntry::cost).sum::<usize>()
        );
    }

    #[test]
    fn long_messages_are_cut_on_a_char_boundary() {
        let ring = LogRing::new();
        // 3-byte chars so the byte limit lands mid-character.
        ring.push(entry(&"€".repeat(MAX_MESSAGE)));
        let got = ring.slice(None, None);
        assert!(got[0].message.len() <= MAX_MESSAGE);
        assert!(got[0].message.ends_with('…'));
    }

    #[test]
    fn layer_records_levels_message_fields_and_timestamp() {
        let ring = LogRing::new();
        let subscriber = tracing_subscriber::registry().with(ring.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("e");
            tracing::warn!("w");
            tracing::info!(port = 4545, name = "x y", "created");
            tracing::debug!("d");
            tracing::trace!("t");
            tracing::info!(target: "rift::script", "secret-from-script");
            // CR/LF stays inside the string; JSON escapes it.
            tracing::info!("a\r\nb");
        });
        let got = ring.slice(None, None);
        let levels: Vec<_> = got.iter().map(|e| e.level).collect();
        assert_eq!(levels, ["error", "warn", "info", "debug", "debug", "info"]);
        assert_eq!(got[2].message, "created port=4545 name=x y");
        assert!(!got.iter().any(|e| e.message.contains("secret")));
        let json = serde_json::to_string(&got[5]).unwrap();
        assert!(json.contains(r"a\r\nb") && !json.contains('\n'), "{json}");
        let shape = regex_lite_timestamp(&got[0].timestamp);
        assert!(shape, "{}", got[0].timestamp);
    }

    /// `^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$`, without a regex dependency.
    fn regex_lite_timestamp(s: &str) -> bool {
        let pattern = "dddd-dd-ddTdd:dd:dd.dddZ";
        s.len() == pattern.len()
            && s.chars().zip(pattern.chars()).all(|(c, p)| match p {
                'd' => c.is_ascii_digit(),
                _ => c == p,
            })
    }
}
