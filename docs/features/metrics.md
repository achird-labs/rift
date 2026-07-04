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
rift-http-proxy --metrics-port 8090
# or
RIFT_METRICS_PORT=8090 rift-http-proxy
```

There is no environment variable to disable the metrics server; if you don't scrape it, it sits
idle. (An imposter's `_rift.metrics` block controls per-imposter metric emission — see
[Rift Extensions]({{ site.baseurl }}/configuration/native/).)

---

## Prometheus endpoint metrics (port 9090)

Histogram metrics expand into the usual `_bucket{le="…"}`, `_sum`, and `_count` series.

| Metric | Type | Labels | Meaning |
|:-------|:-----|:-------|:--------|
| `rift_requests_total` | counter | `method`, `status` | Requests served, by method and response status. |
| `rift_faults_injected_total` | counter | `type`, `rule_id`, `source` | Faults injected (`type` = latency/error/tcp). |
| `rift_latency_injected_ms` | histogram | `rule_id` | Injected latency, in milliseconds. |
| `rift_error_status_total` | counter | `status`, `rule_id` | Error-fault responses, by status. |
| `rift_script_execution_duration_ms` | histogram | `rule_id`, `result` | Script execution time, in milliseconds. |
| `rift_script_errors_total` | counter | `rule_id`, `error_type` | Script failures, by error type. |
| `rift_flow_state_ops_total` | counter | `operation`, `result` | Flow-store operations (get/set/…), by result. |
| `rift_active_flows` | gauge | `backend` | Currently-tracked flows, by backend. |
| `rift_proxy_request_duration_ms` | histogram | `method`, `fault_applied` | Proxy handling time, in milliseconds. |
| `rift_upstream_request_duration_ms` | histogram | `method`, `status` | Upstream (proxied) request time, in milliseconds. |

Example scrape output:

```prometheus
rift_requests_total{method="GET",status="200"} 1234
rift_faults_injected_total{type="latency",rule_id="api-latency",source="rift"} 300
rift_latency_injected_ms_bucket{rule_id="api-latency",le="100"} 120
rift_latency_injected_ms_sum{rule_id="api-latency"} 45670
rift_latency_injected_ms_count{rule_id="api-latency"} 300
rift_flow_state_ops_total{operation="get",result="ok"} 5000
rift_active_flows{backend="inmemory"} 12
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
