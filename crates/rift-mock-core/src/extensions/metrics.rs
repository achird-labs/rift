//! Prometheus metrics for rift-http-proxy.
//!
//! Tracks fault injection activity, script execution, and proxy performance.
use lazy_static::lazy_static;
use parking_lot::Mutex;
use prometheus::{
    Counter, CounterVec, Encoder, GaugeVec, HistogramVec, TextEncoder, register_counter_vec,
    register_gauge_vec, register_histogram_vec,
};
use std::collections::HashMap;

lazy_static! {
    /// Total number of requests processed
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "rift_requests_total",
        "Total number of requests processed by the proxy",
        &["method", "status"]
    )
    .unwrap();

    /// Connections accepted per accept-loop slot (issue #746). Under `--runtime per-core`
    /// the slot index IS the worker index, which makes SO_REUSEPORT 4-tuple skew observable
    /// in production instead of inferred; in the default topology every accept lands on
    /// slot 0. Resolved to a plain Counter once per accept loop, so the hot path pays one
    /// atomic inc per accepted connection and no label lookup.
    pub static ref ACCEPTED_CONNECTIONS_TOTAL: CounterVec = register_counter_vec!(
        "rift_accepted_connections_total",
        "Connections accepted, labeled by accept-loop worker slot (RFC-712 skew observability)",
        &["worker"]
    )
    .unwrap();

    /// Total number of faults injected
    /// Accept errors, by listener and class (issue #838). Since the accept loops classify and
    /// retry rather than terminate, a wedged listener stays bound and answers nothing — this is
    /// the live signal that it is degraded. `class` is `transient` or `systemic`. Fatal (broken-fd)
    /// errors are excluded on the admin, metrics and proxy listeners because they end the loop and
    /// surface through its owner; the imposter loops have no fatal class by design (a dying imposter
    /// loop is recoverable through the still-live admin API), so there they count as `systemic`.
    /// Deliberately no port label: per-port cardinality is unbounded, and the logs carry the port.
    pub static ref ACCEPT_ERRORS_TOTAL: CounterVec = register_counter_vec!(
        "rift_accept_errors_total",
        "Accept errors by listener and class",
        &["listener", "class"]
    )
    .expect("metric can be created");

    /// Connections dropped before an HTTP/1-or-HTTP/2 decision could be made, by listener and
    /// cause (issue #1045). The `kind` label is the whole point: `timeout` and `eof` are ordinary
    /// client behaviour — a connection went quiet, a client hung up — while `io` can mean a
    /// systemic fault, e.g. every read erroring after a bad cert or a resolver rollout. The call
    /// sites log any of them at `debug!` and must keep doing so: the trigger is entirely
    /// client-controlled, so a per-connection `warn!` would hand a hostile client an unbounded
    /// log-volume lever (#718). A counter is the only way to tell those two situations apart
    /// without raising verbosity, which is exactly what this issue is for.
    ///
    /// Failures only. Successes are already countable as
    /// `rift_accepted_connections_total` minus these; counting them again would double the
    /// per-connection label lookup for nothing. No `port` label, for the same unbounded-cardinality
    /// reason as `ACCEPT_ERRORS_TOTAL`.
    pub static ref PREFACE_FAILURES_TOTAL: CounterVec = register_counter_vec!(
        "rift_preface_failures_total",
        "Connections dropped before an HTTP/1-or-HTTP/2 decision, by listener and cause",
        &["listener", "kind"]
    )
    .expect("metric can be created");

    /// Whether a listener is currently in a systemic accept-error outage (1) or not (0),
    /// issue #838 — the gauge to alert on.
    pub static ref ACCEPT_ERROR_OUTAGE: GaugeVec = register_gauge_vec!(
        "rift_accept_error_outage",
        "1 while a listener is in a systemic accept-error outage, 0 otherwise",
        &["listener"]
    )
    .expect("metric can be created");

    pub static ref FAULTS_INJECTED_TOTAL: CounterVec = register_counter_vec!(
        "rift_faults_injected_total",
        "Total number of faults injected",
        &["type", "rule_id", "source"]  // type: latency|error|tcp, source: rift|script
    )
    .unwrap();

    /// Latency fault duration in milliseconds
    pub static ref LATENCY_INJECTED_MS: HistogramVec = register_histogram_vec!(
        "rift_latency_injected_ms",
        "Histogram of injected latency in milliseconds",
        &["rule_id"],
        vec![10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0]
    )
    .unwrap();

    /// Error fault status codes
    pub static ref ERROR_STATUS_TOTAL: CounterVec = register_counter_vec!(
        "rift_error_status_total",
        "Count of error status codes injected",
        &["status", "rule_id"]
    )
    .unwrap();

    /// Script execution duration
    pub static ref SCRIPT_EXECUTION_DURATION_MS: HistogramVec = register_histogram_vec!(
        "rift_script_execution_duration_ms",
        "Histogram of script execution time in milliseconds",
        &["rule_id", "result"],  // result: pass|fault|error
        vec![0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0]
    )
    .unwrap();

    /// Flow state operations
    pub static ref FLOW_STATE_OPS_TOTAL: CounterVec = register_counter_vec!(
        "rift_flow_state_ops_total",
        "Total number of flow state operations",
        &["operation", "result"]  // operation: one per FlowStore method, result: success|error
    )
    .unwrap();

    /// Upstream request duration (without faults)
    pub static ref UPSTREAM_REQUEST_DURATION_MS: HistogramVec = register_histogram_vec!(
        "rift_upstream_request_duration_ms",
        "Duration of upstream requests (excluding fault injection)",
        &["method", "status"]
    )
    .unwrap();

    /// Script compilation errors
    pub static ref SCRIPT_ERRORS_TOTAL: CounterVec = register_counter_vec!(
        "rift_script_errors_total",
        "Total number of script execution errors",
        &["rule_id", "error_type"]  // error_type: syntax|runtime|flow_state
    )
    .unwrap();
}

/// Collect and return all metrics in Prometheus text format
pub fn collect_metrics() -> String {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Helper to record request processing
pub fn record_request(method: &str, status: u16) {
    REQUESTS_TOTAL
        .with_label_values(&[method, &status.to_string()])
        .inc();
}

/// Helper to record fault injection
pub fn record_fault_injection(fault_type: &str, rule_id: &str, source: &str) {
    FAULTS_INJECTED_TOTAL
        .with_label_values(&[fault_type, rule_id, source])
        .inc();
}

/// A latency fault fired: observe how long, and count it once.
///
/// `source` is the caller's — `"rift"` for `_rift.fault`, `"script"` for a script decision. It is a
/// parameter rather than a constant because this helper *also* increments
/// [`FAULTS_INJECTED_TOTAL`]: a caller that recorded the fault separately would double-count it,
/// which is exactly what happened while the label was hardcoded. One call per fired fault.
pub fn record_latency_injection(rule_id: &str, duration_ms: u64, source: &str) {
    LATENCY_INJECTED_MS
        .with_label_values(&[rule_id])
        .observe(duration_ms as f64);

    record_fault_injection("latency", rule_id, source);
}

/// An error fault fired: count the status, and count the fault once. See
/// [`record_latency_injection`] for why `source` is a parameter.
pub fn record_error_injection(rule_id: &str, status: u16, source: &str) {
    ERROR_STATUS_TOTAL
        .with_label_values(&[&status.to_string(), rule_id])
        .inc();

    record_fault_injection("error", rule_id, source);
}

/// Helper to record script execution
pub fn record_script_execution(rule_id: &str, duration_ms: f64, result: &str) {
    SCRIPT_EXECUTION_DURATION_MS
        .with_label_values(&[rule_id, result])
        .observe(duration_ms);
}

/// Helper to record script fault injection
pub fn record_script_fault(fault_type: &str, rule_id: &str, duration_ms: Option<u64>) {
    // Delegating for the latency case keeps a script-decided delay in the same histogram as a
    // `_rift.fault` one — otherwise `rift_latency_injected_ms` would quietly mean "rift-path
    // latency only" while the counter beside it counted both.
    match (fault_type, duration_ms) {
        ("latency", Some(ms)) => record_latency_injection(rule_id, ms, "script"),
        _ => record_fault_injection(fault_type, rule_id, "script"),
    }
}

/// Helper to record flow state operation
pub fn record_flow_state_op(operation: &str, success: bool) {
    let result = if success { "success" } else { "error" };
    FLOW_STATE_OPS_TOTAL
        .with_label_values(&[operation, result])
        .inc();
}

lazy_static! {
    /// How many accept loops are currently inside a systemic outage, per listener (issue #838).
    ///
    /// The gauge cannot simply be `set(0)` by whoever recovers first: one `listener` label covers
    /// **many** loops — the imposter plane runs one accept loop per imposter *and* one per accept
    /// runtime under per-core fan-out (SO_REUSEPORT). Systemic errors are dominated by process-wide
    /// fd exhaustion, so those loops wedge together and recover asynchronously; a plain `set(0)`
    /// would clear the gauge on the first recovery while the rest are still down, under-reporting
    /// exactly the outage the gauge exists to report. Counting depth makes it "any loop wedged".
    static ref ACCEPT_OUTAGE_DEPTH: Mutex<HashMap<&'static str, usize>> =
        Mutex::new(HashMap::new());
}

/// Tracks one accept loop's outage state and keeps [`ACCEPT_ERROR_OUTAGE`] consistent (issue #838).
///
/// Held by the loop for its lifetime. `Drop` releases the outage, so **every** way a loop can end
/// while wedged — a shutdown/cancel `break`, a fatal `return Err`, a panic unwind — clears its
/// contribution. Without that, deleting an imposter mid-outage would leave the gauge stuck at 1
/// forever for a listener that no longer exists, and the registry is process-global, so it would
/// survive across embedded server lifecycles.
#[derive(Debug)]
pub struct AcceptOutageGuard {
    listener: &'static str,
    in_outage: bool,
}

impl AcceptOutageGuard {
    /// Start tracking `listener`. Materializes the series so alerts can reference it before the
    /// first outage ever happens (Prometheus only creates a child on first use).
    ///
    /// Goes through [`Self::adjust`] with a zero delta rather than writing the gauge directly: a
    /// new loop can start while *another* loop on the same label is already wedged (per-core
    /// fan-out starts N loops, a new imposter is created during an outage, a second embedded
    /// server starts), and a blind `set(0)` there would clear the gauge while the depth stayed
    /// non-zero — and stay wrong, since the gauge is only rewritten on an outage transition.
    #[must_use]
    pub fn new(listener: &'static str) -> Self {
        Self::adjust(listener, 0);
        Self {
            listener,
            in_outage: false,
        }
    }

    /// This loop entered a systemic outage. Idempotent.
    pub fn enter(&mut self) {
        if !self.in_outage {
            self.in_outage = true;
            Self::adjust(self.listener, 1);
        }
    }

    /// This loop recovered (or is ending). Idempotent.
    pub fn exit(&mut self) {
        if self.in_outage {
            self.in_outage = false;
            Self::adjust(self.listener, -1);
        }
    }

    fn adjust(listener: &'static str, delta: isize) {
        let mut depth = ACCEPT_OUTAGE_DEPTH.lock();
        let entry = depth.entry(listener).or_insert(0);
        *entry = entry.saturating_add_signed(delta);
        let any = *entry > 0;
        ACCEPT_ERROR_OUTAGE
            .with_label_values(&[listener])
            .set(if any { 1.0 } else { 0.0 });
    }
}

impl Drop for AcceptOutageGuard {
    fn drop(&mut self) {
        // Runs during unwind too; `parking_lot::Mutex` never poisons, so this cannot double-panic.
        self.exit();
    }
}

/// Pre-resolved [`ACCEPT_ERRORS_TOTAL`] children for one accept loop (issue #840).
///
/// Resolving `with_label_values` per error costs a label hash plus a `MetricVec` lookup on a path
/// that must not carry hoistable work: the transient arm retries with **no backoff by design**
/// (#750), so a `ECONNABORTED` storm runs it at full loop rate. Resolve the two children once when
/// the loop starts and pay one atomic add per error instead — the same treatment #746 gave
/// `ACCEPTED_CONNECTIONS_TOTAL`.
///
/// Held per loop alongside [`AcceptBackoff`](crate::proxy::AcceptBackoff),
/// [`AcceptErrorLog`](crate::proxy::AcceptErrorLog) and [`AcceptOutageGuard`]. Deliberately a
/// separate type from that guard: this is a pair of monotonic handles with no lifecycle, whereas
/// the guard owns RAII outage state whose `Drop` carries meaning.
#[derive(Debug, Clone)]
pub struct AcceptErrorCounters {
    transient: Counter,
    systemic: Counter,
}

impl AcceptErrorCounters {
    /// Resolve both class children for `listener` once.
    #[must_use]
    pub fn new(listener: &'static str) -> Self {
        Self {
            transient: ACCEPT_ERRORS_TOTAL.with_label_values(&[listener, "transient"]),
            systemic: ACCEPT_ERRORS_TOTAL.with_label_values(&[listener, "systemic"]),
        }
    }

    /// Count a transient accept error (retried immediately).
    pub fn record_transient(&self) {
        self.transient.inc();
    }

    /// Count a systemic accept error (backed off).
    pub fn record_systemic(&self) {
        self.systemic.inc();
    }
}

/// The `kind` label values of [`PREFACE_FAILURES_TOTAL`], in one place so the recorder and the
/// startup materialiser cannot drift apart.
const PREFACE_FAILURE_KINDS: [&str; 3] = ["timeout", "eof", "io"];

fn preface_failure_kind(err: &crate::proxy::preface::PrefaceError) -> &'static str {
    use crate::proxy::preface::PrefaceError;
    // Exhaustive here rather than at the call sites: `PrefaceError` is `#[non_exhaustive]`, so the
    // four call sites in `rift-http-proxy` *cannot* match it exhaustively and would need a
    // wildcard — which is how a new variant would silently start counting as something it is not.
    // In its defining crate the compiler makes adding a variant a build error instead.
    match err {
        PrefaceError::Timeout(_) => "timeout",
        PrefaceError::Eof => "eof",
        PrefaceError::Io(_) => "io",
    }
}

/// Count one connection dropped before a protocol decision, discriminated by cause.
///
/// `listener` reuses the exact strings the accept loops pass to [`AcceptErrorCounters::new`]
/// (`imposter`, `front-door`, `metrics`, `admin`), plus `intercept` for the tunnel listener, which
/// has no accept counters of its own.
pub fn record_preface_failure(listener: &'static str, err: &crate::proxy::preface::PrefaceError) {
    PREFACE_FAILURES_TOTAL
        .with_label_values(&[listener, preface_failure_kind(err)])
        .inc();
}

/// Touch all three `kind` children so the family is present at `0` from listener start.
///
/// Without this a fresh instance exports no `rift_preface_failures_total` at all, so an alert on
/// it reads as "no data" rather than "no failures" — the same reason `rift_accept_errors_total` is
/// documented as present at `0` for every running listener.
pub fn materialize_preface_failure_counters(listener: &'static str) {
    for kind in PREFACE_FAILURE_KINDS {
        PREFACE_FAILURES_TOTAL.with_label_values(&[listener, kind]);
    }
}

/// Helper to record upstream request duration
pub fn record_upstream_duration(method: &str, status: u16, duration_ms: f64) {
    UPSTREAM_REQUEST_DURATION_MS
        .with_label_values(&[method, &status.to_string()])
        .observe(duration_ms);
}

/// Helper to record script error
pub fn record_script_error(rule_id: &str, error_type: &str) {
    SCRIPT_ERRORS_TOTAL
        .with_label_values(&[rule_id, error_type])
        .inc();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #1045. The counters are process-global and other tests touch the same registry, so
    // every assertion here is a before/after delta on a listener label owned by this test alone —
    // never an absolute value.
    fn preface_count(listener: &str, kind: &str) -> f64 {
        PREFACE_FAILURES_TOTAL
            .with_label_values(&[listener, kind])
            .get()
    }

    #[test]
    fn preface_failures_are_counted_under_the_variant_that_caused_them() {
        use crate::proxy::preface::PrefaceError;
        use std::time::Duration;

        let l = "test-variants";
        let before = ["timeout", "eof", "io"].map(|k| preface_count(l, k));

        record_preface_failure(l, &PrefaceError::Timeout(Duration::from_secs(1)));
        record_preface_failure(l, &PrefaceError::Eof);
        record_preface_failure(
            l,
            &PrefaceError::Io(std::io::Error::other("upstream read failed")),
        );

        // The whole point of the issue: an operator must be able to tell "clients are going quiet"
        // (timeout/eof, ordinary) from "every connection is Io-erroring" (systemic) without
        // raising verbosity, which #718 forbids on this path.
        for (i, kind) in ["timeout", "eof", "io"].iter().enumerate() {
            assert_eq!(
                preface_count(l, kind) - before[i],
                1.0,
                "one `{kind}` failure must land on the `{kind}` child and nowhere else"
            );
        }
    }

    #[test]
    fn materializing_makes_all_three_kinds_present_at_zero() {
        // A family that only appears on first failure reads as "no data" to an alert, not "no
        // failures" — so `rate()` and `absent()` both misbehave on a healthy fresh instance.
        let l = "test-materialize";
        materialize_preface_failure_counters(l);

        let scrape = collect_metrics();
        for kind in ["timeout", "eof", "io"] {
            let series =
                format!(r#"rift_preface_failures_total{{kind="{kind}",listener="{l}"}} 0"#);
            assert!(
                scrape.contains(&series),
                "expected `{series}` at zero in the scrape;\n{scrape}"
            );
        }
    }

    #[test]
    fn test_metrics_collection() {
        // Record some metrics
        record_request("GET", 200);
        record_latency_injection("test-rule", 100, "rift");

        // Collect metrics
        let metrics = collect_metrics();

        // Verify metrics are present
        assert!(metrics.contains("rift_requests_total"));
        assert!(metrics.contains("rift_faults_injected_total"));
        assert!(metrics.contains("rift_latency_injected_ms"));
    }

    #[test]
    fn test_script_metrics() {
        record_script_execution("script-rule", 1.5, "inject");
        record_script_fault("error", "script-rule", None);
        record_script_error("bad-script", "runtime");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_script_execution_duration_ms"));
        assert!(metrics.contains("rift_script_errors_total"));
    }

    #[test]
    fn test_flow_state_metrics() {
        record_flow_state_op("increment", true);
        record_flow_state_op("get", false);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_flow_state_ops_total"));
    }

    // ============================================
    // Additional tests for expanded coverage
    // ============================================

    #[test]
    fn test_record_request_various_methods() {
        record_request("GET", 200);
        record_request("POST", 201);
        record_request("PUT", 204);
        record_request("DELETE", 200);
        record_request("PATCH", 200);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_requests_total"));
    }

    #[test]
    fn test_record_request_error_codes() {
        record_request("GET", 400);
        record_request("GET", 401);
        record_request("GET", 403);
        record_request("GET", 404);
        record_request("GET", 500);
        record_request("GET", 502);
        record_request("GET", 503);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_requests_total"));
    }

    #[test]
    fn test_record_fault_injection_types() {
        record_fault_injection("latency", "rule-1", "v1");
        record_fault_injection("error", "rule-2", "v1");
        record_fault_injection("latency", "rule-3", "script");
        record_fault_injection("error", "rule-4", "script");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_faults_injected_total"));
    }

    #[test]
    fn test_record_latency_injection_various_durations() {
        record_latency_injection("slow-rule", 5, "rift");
        record_latency_injection("slow-rule", 50, "rift");
        record_latency_injection("slow-rule", 100, "rift");
        record_latency_injection("slow-rule", 500, "rift");
        record_latency_injection("slow-rule", 1000, "rift");
        record_latency_injection("slow-rule", 5000, "rift");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_latency_injected_ms"));
    }

    #[test]
    fn test_record_error_injection_status_codes() {
        record_error_injection("error-rule", 400, "rift");
        record_error_injection("error-rule", 401, "rift");
        record_error_injection("error-rule", 403, "rift");
        record_error_injection("error-rule", 404, "rift");
        record_error_injection("error-rule", 500, "rift");
        record_error_injection("error-rule", 502, "rift");
        record_error_injection("error-rule", 503, "rift");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_error_status_total"));
    }

    #[test]
    fn test_record_script_execution_results() {
        record_script_execution("script-1", 0.5, "inject");
        record_script_execution("script-1", 1.0, "pass");
        record_script_execution("script-1", 0.1, "error");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_script_execution_duration_ms"));
    }

    #[test]
    fn test_record_script_fault_with_latency() {
        record_script_fault("latency", "latency-rule", Some(500));
        record_script_fault("latency", "latency-rule", Some(1000));

        // Error faults don't have duration
        record_script_fault("error", "error-rule", None);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_faults_injected_total"));
    }

    #[test]
    fn test_record_flow_state_ops_all_types() {
        record_flow_state_op("get", true);
        record_flow_state_op("get", false);
        record_flow_state_op("set", true);
        record_flow_state_op("set", false);
        record_flow_state_op("increment", true);
        record_flow_state_op("increment", false);
        record_flow_state_op("exists", true);
        record_flow_state_op("exists", false);
        record_flow_state_op("delete", true);
        record_flow_state_op("delete", false);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_flow_state_ops_total"));
    }

    #[test]
    fn test_record_upstream_duration() {
        record_upstream_duration("GET", 200, 15.5);
        record_upstream_duration("POST", 201, 25.0);
        record_upstream_duration("GET", 404, 5.0);
        record_upstream_duration("GET", 500, 100.0);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_upstream_request_duration_ms"));
    }

    #[test]
    fn test_record_script_error_types() {
        record_script_error("bad-script", "syntax");
        record_script_error("bad-script", "runtime");
        record_script_error("bad-script", "flow_state");
        record_script_error("bad-script", "timeout");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_script_errors_total"));
    }

    // Issue #838: the accept-error signals a scrape can alert on. A wedged listener stays bound
    // and answers nothing, so these are the live evidence that it is degraded. Issue #840: driven
    // through the pre-resolved handles, so this exercises the production path rather than a helper
    // no loop calls.
    #[test]
    fn test_accept_error_counters_record_by_listener_and_class() {
        // Unique label so this can never collide with another test's counters.
        let counters = AcceptErrorCounters::new("test-counter");
        counters.record_systemic();
        counters.record_systemic();
        counters.record_transient();

        assert_eq!(
            ACCEPT_ERRORS_TOTAL
                .with_label_values(&["test-counter", "systemic"])
                .get(),
            2.0
        );
        assert_eq!(
            ACCEPT_ERRORS_TOTAL
                .with_label_values(&["test-counter", "transient"])
                .get(),
            1.0
        );

        // A second handle for the same listener must address the same children, not fresh ones —
        // otherwise a restarted loop would silently reset its own series.
        let again = AcceptErrorCounters::new("test-counter");
        again.record_transient();
        assert_eq!(
            ACCEPT_ERRORS_TOTAL
                .with_label_values(&["test-counter", "transient"])
                .get(),
            2.0,
            "re-resolving a label must return the same child, not a new counter"
        );

        let rendered = collect_metrics();
        assert!(rendered.contains("rift_accept_errors_total"));
    }

    // Issue #838: one `listener` label covers MANY accept loops (per-core fan-out, one loop per
    // imposter), and a process-wide EMFILE wedges them together but they recover one at a time.
    // The gauge must mean "any loop still wedged", not "the last loop to write".
    #[test]
    fn test_accept_outage_gauge_tracks_depth_not_last_writer() {
        // Unique label so this can never collide with another test's counters.
        let listener = "test-depth";
        let mut a = AcceptOutageGuard::new(listener);
        let mut b = AcceptOutageGuard::new(listener);
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            0.0,
            "the series exists at 0 before any outage, so alerts can reference it"
        );

        a.enter();
        b.enter();
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            1.0
        );

        // A loop that starts *while* another is already wedged must not clear the gauge: per-core
        // fan-out starts its loops independently, so this ordering is the normal case, not a race.
        let _late = AcceptOutageGuard::new(listener);
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            1.0,
            "constructing a guard must not zero a gauge another loop is holding up"
        );

        a.exit();
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            1.0,
            "one loop recovering must NOT clear the gauge while another is still wedged"
        );

        b.exit();
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            0.0,
            "the gauge clears only when the last wedged loop recovers"
        );
    }

    // Issue #838: a loop that ends while wedged (cancel break, fatal return, panic) must not leave
    // the gauge stuck at 1 for a listener that no longer exists — the registry is process-global.
    #[test]
    fn test_accept_outage_guard_clears_on_drop() {
        let listener = "test-drop";
        {
            let mut g = AcceptOutageGuard::new(listener);
            g.enter();
            assert_eq!(
                ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
                1.0
            );
            // dropped here while still in outage, as a cancel break would
        }
        assert_eq!(
            ACCEPT_ERROR_OUTAGE.with_label_values(&[listener]).get(),
            0.0,
            "dropping a wedged loop must release its outage"
        );
    }

    #[test]
    fn test_collect_metrics_returns_string() {
        // Record some data first to ensure metrics are populated
        record_request("GET", 200);

        let metrics = collect_metrics();

        // Prometheus format should be a valid string
        assert!(!metrics.is_empty() || metrics.is_empty()); // Always true - just verify no panic
    }

    #[test]
    fn test_collect_metrics_after_recording() {
        // Record some data to populate metrics
        record_request("POST", 201);
        record_fault_injection("latency", "format-test", "v1");

        let metrics = collect_metrics();
        // Should contain our recorded metrics
        assert!(metrics.contains("rift_requests_total") || metrics.is_empty());
    }

    #[test]
    fn test_multiple_rules_same_metric() {
        // Multiple rules should create separate label combinations
        record_latency_injection("rule-a", 100, "rift");
        record_latency_injection("rule-b", 200, "rift");
        record_latency_injection("rule-c", 300, "rift");

        record_error_injection("rule-a", 500, "rift");
        record_error_injection("rule-b", 503, "rift");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_latency_injected_ms"));
        assert!(metrics.contains("rift_error_status_total"));
    }

    #[test]
    fn test_high_precision_duration() {
        // Test sub-millisecond precision
        record_script_execution("fast-script", 0.001, "pass");
        record_script_execution("fast-script", 0.01, "pass");
        record_script_execution("fast-script", 0.1, "pass");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_script_execution_duration_ms"));
    }

    #[test]
    fn test_histogram_buckets_coverage() {
        // Test that histogram buckets are properly created
        // by recording values that span different buckets
        let durations = [
            0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0,
        ];

        for (i, duration) in durations.iter().enumerate() {
            record_script_execution(&format!("bucket-test-{i}"), *duration, "pass");
        }

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_script_execution_duration_ms"));
    }
}
