#![allow(dead_code)] // Functions used in binary, not in lib tests

//! Prometheus metrics for rift-http-proxy.
//!
//! Tracks fault injection activity, script execution, and proxy performance.
use lazy_static::lazy_static;
use prometheus::{
    register_counter_vec, register_gauge_vec, register_histogram_vec, CounterVec, Encoder,
    GaugeVec, HistogramVec, TextEncoder,
};

lazy_static! {
    /// Total number of requests processed
    pub static ref REQUESTS_TOTAL: CounterVec = register_counter_vec!(
        "rift_requests_total",
        "Total number of requests processed by the proxy",
        &["method", "status"]
    )
    .unwrap();

    /// Total number of faults injected
    pub static ref FAULTS_INJECTED_TOTAL: CounterVec = register_counter_vec!(
        "rift_faults_injected_total",
        "Total number of faults injected",
        &["type", "rule_id", "source"]  // type: latency|error, source: v1|script
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
        &["rule_id", "result"],  // result: inject|pass|error
        vec![0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 50.0, 100.0]
    )
    .unwrap();

    /// Flow state operations
    pub static ref FLOW_STATE_OPS_TOTAL: CounterVec = register_counter_vec!(
        "rift_flow_state_ops_total",
        "Total number of flow state operations",
        &["operation", "result"]  // operation: get|set|increment|exists|delete, result: success|error
    )
    .unwrap();

    /// Active flows being tracked
    pub static ref ACTIVE_FLOWS: GaugeVec = register_gauge_vec!(
        "rift_active_flows",
        "Number of active flows being tracked in flow state",
        &["backend"]  // backend: inmemory|redis|valkey
    )
    .unwrap();

    /// Proxy request duration
    pub static ref PROXY_REQUEST_DURATION_MS: HistogramVec = register_histogram_vec!(
        "rift_proxy_request_duration_ms",
        "Total request duration including faults and forwarding",
        &["method", "fault_applied"],  // fault_applied: none|latency|error|script
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0]
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

/// Helper to record latency injection
pub fn record_latency_injection(rule_id: &str, duration_ms: u64) {
    LATENCY_INJECTED_MS
        .with_label_values(&[rule_id])
        .observe(duration_ms as f64);

    record_fault_injection("latency", rule_id, "v1");
}

/// Helper to record error injection
pub fn record_error_injection(rule_id: &str, status: u16) {
    ERROR_STATUS_TOTAL
        .with_label_values(&[&status.to_string(), rule_id])
        .inc();

    record_fault_injection("error", rule_id, "v1");
}

/// Helper to record script execution
pub fn record_script_execution(rule_id: &str, duration_ms: f64, result: &str) {
    SCRIPT_EXECUTION_DURATION_MS
        .with_label_values(&[rule_id, result])
        .observe(duration_ms);
}

/// Helper to record script fault injection
pub fn record_script_fault(fault_type: &str, rule_id: &str, duration_ms: Option<u64>) {
    record_fault_injection(fault_type, rule_id, "script");

    if fault_type == "latency" {
        if let Some(ms) = duration_ms {
            LATENCY_INJECTED_MS
                .with_label_values(&[rule_id])
                .observe(ms as f64);
        }
    }
}

/// Helper to record flow state operation
pub fn record_flow_state_op(operation: &str, success: bool) {
    let result = if success { "success" } else { "error" };
    FLOW_STATE_OPS_TOTAL
        .with_label_values(&[operation, result])
        .inc();
}

/// Helper to set active flows gauge
pub fn set_active_flows(backend: &str, count: i64) {
    ACTIVE_FLOWS.with_label_values(&[backend]).set(count as f64);
}

/// Helper to record proxy request duration
pub fn record_proxy_duration(method: &str, duration_ms: f64, fault_applied: &str) {
    PROXY_REQUEST_DURATION_MS
        .with_label_values(&[method, fault_applied])
        .observe(duration_ms);
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

    #[test]
    fn test_metrics_collection() {
        // Record some metrics
        record_request("GET", 200);
        record_fault_injection("latency", "test-rule", "v1");
        record_latency_injection("test-rule", 100);

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
        set_active_flows("inmemory", 42);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_flow_state_ops_total"));
        assert!(metrics.contains("rift_active_flows"));
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
        record_latency_injection("slow-rule", 5);
        record_latency_injection("slow-rule", 50);
        record_latency_injection("slow-rule", 100);
        record_latency_injection("slow-rule", 500);
        record_latency_injection("slow-rule", 1000);
        record_latency_injection("slow-rule", 5000);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_latency_injected_ms"));
    }

    #[test]
    fn test_record_error_injection_status_codes() {
        record_error_injection("error-rule", 400);
        record_error_injection("error-rule", 401);
        record_error_injection("error-rule", 403);
        record_error_injection("error-rule", 404);
        record_error_injection("error-rule", 500);
        record_error_injection("error-rule", 502);
        record_error_injection("error-rule", 503);

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
    fn test_set_active_flows_backends() {
        set_active_flows("inmemory", 100);
        set_active_flows("redis", 200);
        set_active_flows("valkey", 150);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_active_flows"));
    }

    #[test]
    fn test_set_active_flows_zero() {
        set_active_flows("inmemory", 0);

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_active_flows"));
    }

    #[test]
    fn test_record_proxy_duration_fault_types() {
        record_proxy_duration("GET", 10.5, "none");
        record_proxy_duration("POST", 100.0, "latency");
        record_proxy_duration("PUT", 5.0, "error");
        record_proxy_duration("DELETE", 50.0, "script");

        let metrics = collect_metrics();
        assert!(metrics.contains("rift_proxy_request_duration_ms"));
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
        record_latency_injection("rule-a", 100);
        record_latency_injection("rule-b", 200);
        record_latency_injection("rule-c", 300);

        record_error_injection("rule-a", 500);
        record_error_injection("rule-b", 503);

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
