@rift @metrics
Feature: Metrics — the Prometheus families on --metrics-port and the admin GET /metrics

  # rift docs/features/metrics.md (v0.18.1); 0.18.0 #999 made nine documented families actually be
  # written, #1045 added the preface/accept families. Every assertion is a DELTA keyed by the
  # scenario's own imposter (`rule_id` is its port, `{port}` below), so the shared engine's other
  # traffic cannot move it. Selectors are `name{label=value,…}` with unquoted values and match one
  # series exactly (a `_created` sibling is never summed in). Every literal was probed with curl
  # against a standalone 0.18.1 engine first (rift-conformance#192).
  #
  # `@metrics-port` scenarios read the separate Prometheus listener, which the embedded harness does
  # not have (DESIGN §3.1), so engineEmbedded excludes them; the admin /metrics ones run everywhere.

  # Present at 0 from startup for every running listener (#1045) — the imposter listener's series
  # exist once one is bound.
  @metrics-port
  Scenario Outline: <series> is exported before anything happens
    Given a fresh imposter from fixture "metrics/plain"
    Then the metrics listener exports "<series>"

    Examples:
      | series                                                            |
      | rift_preface_failures_total{listener=imposter,kind=eof}           |
      | rift_preface_failures_total{listener=admin,kind=timeout}          |
      | rift_accept_errors_total{listener=admin,class=transient}          |
      | rift_accept_error_outage{listener=admin}                          |

  @metrics-port
  Scenario: a healthy listener reports no accept-error outage
    Given a fresh imposter from fixture "metrics/plain"
    Then the metrics listener reports "rift_accept_error_outage{listener=admin}" as 0

  @metrics-port
  Scenario: a latency fault counts the fault and the injected-latency histogram
    Given a fresh imposter from fixture "metrics/latency"
    When the client snapshots the "metrics" listener
    And the client sends "GET" "/"
    And the client sends "GET" "/"
    Then on the "metrics" listener "rift_faults_injected_total{type=latency,rule_id={port},source=rift}" grew by 2
    And on the "metrics" listener "rift_latency_injected_ms_count{rule_id={port}}" grew by 2

  @metrics-port
  Scenario: an error fault counts the fault and its status
    Given a fresh imposter from fixture "metrics/error"
    When the client snapshots the "metrics" listener
    And the client sends "GET" "/"
    Then the response status is 503
    And on the "metrics" listener "rift_faults_injected_total{type=error,rule_id={port},source=rift}" grew by 1
    And on the "metrics" listener "rift_error_status_total{status=503,rule_id={port}}" grew by 1

  @metrics-port
  Scenario: a tcp fault counts as type tcp
    Given a fresh imposter from fixture "metrics/tcp"
    When the client snapshots the "metrics" listener
    And the client sends "POST" "/" expecting a transport error
    Then on the "metrics" listener "rift_faults_injected_total{type=tcp,rule_id={port},source=rift}" grew by 1

  # `result` is `pass` only for `pass()` — the doc's "no fault". A script that answers with
  # `http(...)` has decided the response, so it counts as a script fault, not a pass.
  @metrics-port
  Scenario: script outcomes are counted by result and error type
    Given a fresh imposter from fixture "metrics/scripts"
    When the client snapshots the "metrics" listener
    And the client sends "GET" "/pass"
    And the client sends "GET" "/throw"
    And the client sends "POST" "/reset" expecting a transport error
    Then on the "metrics" listener "rift_script_execution_duration_ms_count{rule_id={port},result=pass}" grew by 1
    And on the "metrics" listener "rift_script_errors_total{rule_id={port},error_type=runtime}" grew by 1
    And on the "metrics" listener "rift_script_execution_duration_ms_count{rule_id={port},result=error}" grew by 1
    And on the "metrics" listener "rift_faults_injected_total{type=tcp,rule_id={port},source=script}" grew by 1

  # A global counter (no rule_id), so other traffic may add to it: at least, not exactly.
  @metrics-port
  Scenario: a flow-state increment is counted
    Given a fresh imposter from fixture "metrics/scripts"
    When the client snapshots the "metrics" listener
    And the client sends "GET" "/incr"
    Then the response status is 200
    And on the "metrics" listener "rift_flow_state_ops_total{operation=increment,result=success}" grew by at least 1

  @metrics-port
  Scenario: a proxied request is timed as an upstream request
    Given an upstream imposter from fixture "metrics/plain"
    And a fresh imposter from fixture "metrics/proxy" proxying to the upstream
    When the client snapshots the "metrics" listener
    And the client sends "GET" "/"
    Then the response status is 200
    And on the "metrics" listener "rift_upstream_request_duration_ms_count{method=GET,status=200}" grew by at least 1

  # #1045: a client that connects and hangs up before sending a byte is a preface `eof`.
  @metrics-port
  Scenario: a connection closed before any request counts as a preface eof
    Given a fresh imposter from fixture "metrics/plain"
    When the client snapshots the "metrics" listener
    And a raw TCP connection to the current imposter closes without sending
    Then on the "metrics" listener "rift_preface_failures_total{listener=imposter,kind=eof}" grew by at least 1

  # ── admin GET /metrics (every leg) ──────────────────────────────────────────────────────────────
  Scenario: admin /metrics is Prometheus text with the imposter count
    Given a fresh imposter from fixture "metrics/plain"
    When the client sends admin "GET" "/metrics" with payload "-"
    Then the response status is 200
    And the response header "content-type" is "text/plain; version=0.0.4"
    And admin metric "rift_imposters_total" equals the number of imposters listed by GET /imposters

  Scenario: admin /metrics counts requests per imposter port
    Given a fresh imposter from fixture "metrics/plain"
    When the client snapshots the "admin" listener
    And the client sends "GET" "/"
    Then on the "admin" listener "rift_imposter_requests_total{port={port}}" grew by 1
