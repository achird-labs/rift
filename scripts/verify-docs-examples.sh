#!/usr/bin/env bash
#
# Docs example-execution gate (issue #436 — Section E2 of the docs umbrella #280).
#
# Boots a freshly built `rift` binary against each documented example config, drives at least one
# HTTP request per imposter through the data plane, and asserts the documented response
# (status + body substring). This complements the reference-coverage gate
# (`verify-docs-coverage.sh`, E1), which only proves every flag/env/key is *mentioned* — not that
# any documented config actually loads and serves. If a documented example rots (stops parsing,
# fails to bind, or serves a different response), CI goes red.
#
# HTTP-level assertions only (per the issue): `rift-verify` ignores `not`/`xpath`, so we assert
# with curl against the imposter data-plane ports instead. `curl -k` covers the HTTPS example
# without cert plumbing, and a generous `--max-time` absorbs the demos' latency-fault behaviors.
#
# Source of truth: the real files under `docs/demo/*.json` (not inline doc JSON). To gate a new
# example, add its file there and add one CASES row per imposter below. Externally-dependent
# configs are intentionally excluded (see EXCLUDED).
#
# Usage:
#   scripts/verify-docs-examples.sh              # build/use rift, run the gate (exit 1 on any failure)
#   scripts/verify-docs-examples.sh --self-test  # prove the gate catches a broken example
#
# Env overrides:
#   RIFT_BIN=/path/to/rift   reuse a prebuilt binary instead of `cargo build`
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEMO_DIR="${DEMO_DIR:-$repo_root/docs/demo}"
ADMIN_PORT="${ADMIN_PORT:-2525}"

# One row per checked request: <config-file>|<url>|<expected-status>|<expected-body-substring>.
# Every self-contained imposter in the corpus is covered by at least one row.
CASES=(
  "imposters.json|http://127.0.0.1:4545/health|200|OK"
  "imposters.json|http://127.0.0.1:4545/api/users|200|Alice"
  "imposters.json|http://127.0.0.1:4546/health|200|OK"
  "imposters.json|http://127.0.0.1:4546/api/orders|200|ORD-001"
  "imposters-rift-features.json|http://127.0.0.1:4547/api/slow-random|200|random latency"
  "imposters-scripting.json|http://127.0.0.1:4550/api/counter|200|counter"
  "imposters-scripting-engines.json|http://127.0.0.1:4560/rhai/counter|200|rhai"
  "imposters-https.json|https://127.0.0.1:4545/api/test|200|Secure response over HTTPS"
)

# Configs deliberately NOT gated, with the reason (keep in sync with docs/demo).
EXCLUDED="imposters-retry-proxy.json (proxies to an external upstream at http://upstream:8080)"

# Diagnostics go to stderr so stdout stays clean for the failure-count that run_config_cases emits.
log()  { echo "[docs-examples] $*" >&2; }
fail() { echo "[FAIL] $*" >&2; }

# Resolve the rift binary: an explicit RIFT_BIN wins; otherwise build the debug binary once.
resolve_rift() {
  if [ -n "${RIFT_BIN:-}" ]; then
    [ -x "$RIFT_BIN" ] || { fail "RIFT_BIN=$RIFT_BIN is not executable"; exit 1; }
    log "using prebuilt binary: $RIFT_BIN"
    return
  fi
  log "building rift (cargo build -p rift-http-proxy)…"
  ( cd "$repo_root" && cargo build -p rift-http-proxy >&2 )
  RIFT_BIN="$repo_root/target/debug/rift-http-proxy"
  [ -x "$RIFT_BIN" ] || { fail "built binary not found at $RIFT_BIN"; exit 1; }
}

RIFT_PID=""
stop_rift() {
  if [ -n "$RIFT_PID" ] && kill -0 "$RIFT_PID" 2>/dev/null; then
    kill "$RIFT_PID" 2>/dev/null || true
    # Bounded teardown: brief grace, then SIGKILL, so a wedged process can never hang the gate.
    local _i
    for _i in 1 2 3 4 5 6; do kill -0 "$RIFT_PID" 2>/dev/null || break; sleep 0.5; done
    kill -9 "$RIFT_PID" 2>/dev/null || true
    wait "$RIFT_PID" 2>/dev/null || true
  fi
  RIFT_PID=""
}
trap stop_rift EXIT

# Start rift with a config file on $ADMIN_PORT and wait until the admin API answers (imposters up).
start_rift() {
  local config="$1"
  "$RIFT_BIN" --configfile "$config" --port "$ADMIN_PORT" >/dev/null 2>&1 &
  RIFT_PID=$!
  local i
  for i in $(seq 1 60); do
    if ! kill -0 "$RIFT_PID" 2>/dev/null; then
      fail "rift exited during startup for $config"; return 1
    fi
    if curl -sf -m 2 "http://127.0.0.1:$ADMIN_PORT/imposters" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  fail "rift admin API did not come up within 30s for $config"; return 1
}

# Drive one request and assert status + body substring. Returns 0 on match, 1 on mismatch.
assert_request() {
  local url="$1" want_status="$2" want_substr="$3" out status body
  # -k: accept the HTTPS demo's self-signed cert. -m 10: absorb latency-fault behaviors.
  # --retry: cheap insurance against a transient connection blip failing the whole gate.
  out="$(curl -sk -m 10 --retry 2 --retry-connrefused -w $'\n%{http_code}' "$url" 2>/dev/null || true)"
  status="${out##*$'\n'}"
  body="${out%$'\n'*}"
  if [ "$status" != "$want_status" ]; then
    fail "$url — expected status $want_status, got '${status:-<none>}'"; return 1
  fi
  if [ -n "$want_substr" ] && [[ "$body" != *"$want_substr"* ]]; then
    fail "$url — response body missing '$want_substr' (got: ${body:0:120})"; return 1
  fi
  log "ok: $url -> $status (matched '${want_substr}')"
  return 0
}

# Run every CASES row for one config against a running rift. Echoes the failure count.
run_config_cases() {
  local config="$1" failures=0 row url st sub
  for row in "${CASES[@]}"; do
    IFS='|' read -r cfg url st sub <<<"$row"
    [ "$cfg" = "$config" ] || continue
    assert_request "$url" "$st" "$sub" || failures=$((failures + 1))
  done
  echo "$failures"
}

# The real gate: for each distinct config, boot rift, run its cases, tear down.
run_gate() {
  resolve_rift
  log "excluded: $EXCLUDED"
  local configs config total_failures=0 f n=0
  # Distinct configs, in first-seen order.
  configs="$(printf '%s\n' "${CASES[@]}" | cut -d'|' -f1 | awk '!seen[$0]++')"
  while IFS= read -r config; do
    [ -n "$config" ] || continue
    # A fresh admin port per config: the admin listener (unlike the imposter ports) binds without
    # SO_REUSEADDR, so reusing one port across sequential boots risks a TIME_WAIT EADDRINUSE.
    ADMIN_PORT=$((2525 + n)); n=$((n + 1))
    log "=== $config (admin :$ADMIN_PORT) ==="
    if ! start_rift "$DEMO_DIR/$config"; then
      total_failures=$((total_failures + 1)); stop_rift; continue
    fi
    f="$(run_config_cases "$config")"
    total_failures=$((total_failures + f))
    stop_rift
  done <<<"$configs"

  if [ "$total_failures" -ne 0 ]; then
    fail "$total_failures documented example assertion(s) failed"
    exit 1
  fi
  log "PASS — every documented example config loads and serves as documented"
}

# Prove the gate is not a no-op: plant a config whose stub returns a known response, then assert
# that a WRONG expectation is caught (non-zero) while the RIGHT one passes.
self_test() {
  resolve_rift
  local tmp; tmp="$(mktemp -d)"
  trap "stop_rift; rm -rf '$tmp'" EXIT

  local test_port=4599 admin=25099
  cat >"$tmp/planted.json" <<JSON
{ "imposters": [ { "port": $test_port, "protocol": "http", "stubs": [
  { "predicates": [{ "equals": { "path": "/probe" } }],
    "responses": [{ "is": { "statusCode": 200, "body": "REAL-BODY" } }] } ] } ] }
JSON

  ADMIN_PORT="$admin"
  start_rift "$tmp/planted.json" || { fail "self-test: rift failed to boot planted config"; exit 1; }

  # Correct expectation must pass.
  assert_request "http://127.0.0.1:$test_port/probe" 200 "REAL-BODY" \
    || { fail "self-test: gate rejected a correct example"; exit 1; }

  # Wrong status must be caught.
  if assert_request "http://127.0.0.1:$test_port/probe" 500 "REAL-BODY" >/dev/null 2>&1; then
    fail "self-test: gate did NOT catch a wrong status — it is a no-op"; exit 1
  fi
  # Wrong body substring must be caught.
  if assert_request "http://127.0.0.1:$test_port/probe" 200 "NOPE-MISSING" >/dev/null 2>&1; then
    fail "self-test: gate did NOT catch a wrong body — it is a no-op"; exit 1
  fi

  stop_rift
  log "PASS — self-test: the gate accepts a correct example and rejects broken ones"
}

case "${1:-}" in
  --self-test) self_test ;;
  "" ) run_gate ;;
  -h | --help) echo "usage: $0 [--self-test]   (RIFT_BIN=/path to reuse a binary)" >&2; exit 64 ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 64 ;;
esac
