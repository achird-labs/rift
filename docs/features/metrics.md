---
layout: default
title: Metrics
parent: Features
nav_order: 4
---

# Prometheus Metrics

Rift exposes two metrics surfaces:

1. A **Prometheus endpoint** (`GET /metrics` on the metrics port, default `9090`) with the full
   instrumentation described below.
2. A small **plain-text counter set** on the admin API's own `GET /metrics` (admin port, default
   `2525`) — imposter counts and per-imposter request totals.

Every metric name and label on this page is taken from the source; if a metric isn't listed here, it
isn't emitted.

---

## Enabling & scraping

The metrics server starts alongside the admin server. Point Prometheus at the metrics port:

```bash
curl http://localhost:9090/metrics
```

Change the port with `--metrics-port` or `RIFT_METRICS_PORT`:

```bash
rift --metrics-port 8090
# or
RIFT_METRICS_PORT=8090 rift
```

There is no environment variable to disable the metrics server; if you don't scrape it, it sits
idle.

---

## Prometheus endpoint metrics (port 9090)

Histogram metrics expand into the usual `_bucket{le="…"}`, `_sum`, and `_count` series.

On every fault and script family below, `rule_id` is the **imposter's port**, as a string: the
imposter path has no named rules the way the removed reverse proxy did, so the port is what
identifies which imposter produced the event. `source` is `rift` for a `_rift.fault` decision
and `script` for one a `_rift.script` returned.

| Metric | Type | Labels | Meaning |
|:-------|:-----|:-------|:--------|
| `rift_requests_total` | counter | `method`, `status` | Requests served, by method and response status. |
| `rift_faults_injected_total` | counter | `type`, `rule_id`, `source` | Faults injected (`type` = latency/error/tcp). |
| `rift_latency_injected_ms` | histogram | `rule_id` | Injected latency, in milliseconds. |
| `rift_error_status_total` | counter | `status`, `rule_id` | Error-fault responses, by status. |
| `rift_script_execution_duration_ms` | histogram | `rule_id`, `result` | Script execution time, in milliseconds. |
| `rift_script_errors_total` | counter | `rule_id`, `error_type` | Script failures, by error type. |
| `rift_flow_state_ops_total` | counter | `operation`, `result` | Flow-store operations (get/set/…), by result. |
| `rift_upstream_request_duration_ms` | histogram | `method`, `status` | Upstream (proxied) request time, in milliseconds. |
| `rift_accepted_connections_total` | counter | `worker` | Connections accepted per accept-loop worker slot. Under `--runtime per-core` the slot is the worker index, making SO_REUSEPORT skew observable; in the default topology everything lands on slot `0`. |
| `rift_accept_errors_total` | counter | `listener`, `class` | Accept errors by listener (`imposter`, `admin`, `metrics`, `front-door`) and class (`transient`, `systemic`). The accept loops classify and retry rather than terminate, so a listener can stay bound while unable to serve — this is the signal that it is degraded. Fatal (broken-fd) errors are excluded on the admin, metrics and proxy listeners — they end the loop and surface through its owner. The imposter loops have no fatal class by design (a dying imposter loop is recoverable through the still-live admin API), so there a broken fd counts as `systemic`. No port label, deliberately (unbounded cardinality); the log lines carry the port. Present at `0` from startup for every running listener, so `rate()`/`increase()` behave on the first error. |
| `rift_preface_failures_total` | counter | `listener`, `kind` | Connections dropped before an HTTP/1-or-HTTP/2 decision could be made, by listener (`imposter`, `admin`, `metrics`, `front-door`, `intercept`) and cause. `kind` is `timeout` (the client completed the transport handshake and then sent nothing in time), `eof` (it hung up first), or `io` (the read itself failed). The distinction is the point: `timeout` and `eof` are ordinary client behaviour, whereas a rising `io` rate is a systemic signal — a bad certificate, a resolver rollout — and the call sites deliberately log all three at `debug!` only, since the trigger is client-controlled and a per-connection warning would be an unbounded log-volume lever (#718). Present at `0` from startup for every running listener, so `rate()`/`increase()` behave on the first failure. |
| `rift_accept_error_outage` | gauge | `listener` | `1` while **any** accept loop for that listener is in a systemic outage, `0` otherwise — the imposter plane runs many loops behind one label, so this counts depth rather than tracking whichever loop wrote last. Released when a loop ends while wedged, so it cannot stick at `1`. Present at `0` from startup, so `absent()` alerts work. The one to alert on. |

Example scrape output:

```prometheus
rift_requests_total{method="GET",status="200"} 1234
rift_faults_injected_total{type="latency",rule_id="4545",source="rift"} 300
rift_faults_injected_total{type="error",rule_id="4545",source="script"} 17
rift_latency_injected_ms_bucket{rule_id="4545",le="100"} 120
rift_latency_injected_ms_sum{rule_id="4545"} 45670
rift_latency_injected_ms_count{rule_id="4545"} 300
rift_error_status_total{status="503",rule_id="4545"} 42
rift_script_execution_duration_ms_count{rule_id="4545",result="fault"} 17
rift_script_errors_total{rule_id="4545",error_type="timeout"} 2
rift_flow_state_ops_total{operation="increment",result="success"} 5000
rift_upstream_request_duration_ms_count{method="GET",status="200"} 88
```

## Admin `GET /metrics` (port 2525)

The admin API serves a minimal hand-written counter set (`Content-Type: text/plain; version=0.0.4`):

| Metric | Type | Labels | Meaning |
|:-------|:-----|:-------|:--------|
| `rift_imposters_total` | gauge | — | Number of imposters currently registered. |
| `rift_imposter_requests_total` | counter | `port` | Requests per imposter (one line per port). |

```prometheus
rift_imposters_total 5
rift_imposter_requests_total{port="4545"} 500
rift_imposter_requests_total{port="4546"} 128
```

---

## Prometheus configuration

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'rift'
    static_configs:
      - targets: ['localhost:9090']
    scrape_interval: 15s
```

### Kubernetes service discovery

```yaml
scrape_configs:
  - job_name: 'rift'
    kubernetes_sd_configs:
      - role: pod
    relabel_configs:
      - source_labels: [__meta_kubernetes_pod_label_app]
        regex: rift
        action: keep
```

---

## Useful queries

**Request rate by status:**
```promql
sum(rate(rift_requests_total[5m])) by (status)
```

**5xx error ratio:**
```promql
sum(rate(rift_requests_total{status=~"5.."}[5m]))
/
sum(rate(rift_requests_total[5m]))
```

**Fault injection rate by type:**
```promql
sum(rate(rift_faults_injected_total[5m])) by (type)
```

**P99 upstream latency (proxy mode):**
```promql
histogram_quantile(0.99, sum(rate(rift_upstream_request_duration_ms_bucket[5m])) by (le))
```

**Script error rate:**
```promql
sum(rate(rift_script_errors_total[5m])) by (error_type)
```

---

## Alerting rules

```yaml
groups:
  - name: rift
    rules:
      - alert: RiftHighErrorRate
        expr: |
          sum(rate(rift_requests_total{status=~"5.."}[5m]))
          /
          sum(rate(rift_requests_total[5m])) > 0.05
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "High 5xx rate in Rift"

      - alert: RiftScriptErrors
        expr: rate(rift_script_errors_total[5m]) > 0
        for: 1m
        labels: { severity: critical }
        annotations:
          summary: "Script errors in Rift"
          description: "error_type={{ $labels.error_type }}"
```

---

## Best practices

1. **Reasonable scrape intervals** — 15–30s is typical.
2. **Use recording rules** for expensive dashboard queries.
3. **Alert on error ratio and fault rate**, not raw counts.
4. **Mind label cardinality** — `rule_id` is bounded by your config; avoid adding unbounded labels.
