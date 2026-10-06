#!/usr/bin/env bash
#
# Vendor-mock template gate (issue #1281).
#
# For every template directory under templates/ (found by glob, so a new template is gated by
# default), boot a freshly built `rift` with `--configfile <entrypoint>` and run the template's own
# `smoke.sh` against it, then stop rift. The cargo tests prove a template parses
# (crates/rift-mock-core/tests/shipped_templates_load.rs) and lints clean (rift-lint's
# `the_shipped_templates_lint_clean`); this gate proves it boots and serves what its smoke test says.
#
# rift is started WITHOUT --allow-injection: a template must be declarative, and the config-file
# door refuses an injecting document without that flag, so a template that grows a script fails here.
#
# smoke.sh contract: curl + python3 only, exits non-zero on the first failed check, reads the admin
# API's base URL from $ADMIN (this gate gives each template its own admin port).
#
# Usage:
#   scripts/verify-templates.sh              # build/use rift, gate every template (exit 1 on any failure)
#   scripts/verify-templates.sh --self-test  # prove the gate rejects broken templates
#
# Env overrides:
#   RIFT_BIN=/path/to/rift        reuse a prebuilt binary instead of `cargo build`
#   TEMPLATES_DIR=/path/to/dir    gate a different catalog (the self-test uses this)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATES_DIR="${TEMPLATES_DIR:-$repo_root/templates}"

log()  { echo "[templates] $*" >&2; }
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
RIFT_LOG=""
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

WORK_DIR=""
cleanup() {
  stop_rift
  [ -n "$WORK_DIR" ] && rm -rf "$WORK_DIR"
  return 0
}
trap cleanup EXIT

# The entrypoint a template's manifest names (relative to the template directory).
entrypoint_of() {
  python3 - "$1/template.json" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    entry = json.load(f).get("entrypoint")
if not isinstance(entry, str) or not entry:
    sys.exit("template.json has no string `entrypoint`")
print(entry)
PY
}

# Start rift on one config with its admin API on $2; wait until the admin API answers.
start_rift() {
  local config="$1" admin_port="$2"
  RIFT_LOG="$WORK_DIR/rift-$admin_port.log"
  "$RIFT_BIN" --configfile "$config" --port "$admin_port" >"$RIFT_LOG" 2>&1 &
  RIFT_PID=$!
  for _ in $(seq 1 60); do
    if ! kill -0 "$RIFT_PID" 2>/dev/null; then
      fail "rift exited during startup for $config:"; tail -n 5 "$RIFT_LOG" >&2 || true
      RIFT_PID=""; return 1
    fi
    if curl -sf -m 2 "http://127.0.0.1:$admin_port/imposters" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.5
  done
  fail "rift admin API did not come up within 30s for $config"; return 1
}

# Gate one template directory: boot, smoke, stop. Returns 0 on pass, 1 on any failure.
check_template() {
  local dir="$1" admin_port="$2" name entry
  name="$(basename "$dir")"
  log "=== $name (admin :$admin_port) ==="
  [ -f "$dir/template.json" ] || { fail "$name: template.json missing"; return 1; }
  [ -f "$dir/smoke.sh" ] || { fail "$name: smoke.sh missing"; return 1; }
  entry="$(entrypoint_of "$dir")" || { fail "$name: unreadable template.json"; return 1; }
  [ -f "$dir/$entry" ] || { fail "$name: entrypoint $entry missing"; return 1; }

  start_rift "$dir/$entry" "$admin_port" || { stop_rift; return 1; }
  local rc=0
  ADMIN="http://localhost:$admin_port" bash "$dir/smoke.sh" >&2 || rc=$?
  stop_rift
  if [ "$rc" -ne 0 ]; then
    fail "$name: smoke.sh exited $rc"; return 1
  fi
  log "ok: $name"
}

# Gate every template under $1, admin ports counting up from $2, setting CATALOG_FAILURES to the
# number that failed; an empty catalog counts as a failure. Called directly, never in `$(...)`: a
# subshell would hold RIFT_PID where the EXIT trap cannot see it, and an interrupt would leave rift
# running on the template's ports.
CATALOG_FAILURES=0
run_catalog() {
  local root="$1" base="$2" dir failures=0 n=0
  for dir in "$root"/*/; do
    [ -d "$dir" ] || continue
    dir="${dir%/}"
    # A fresh admin port per template: the admin listener binds without SO_REUSEADDR, so reusing
    # one port across sequential boots risks a TIME_WAIT EADDRINUSE.
    check_template "$dir" $((base + n)) || failures=$((failures + 1))
    n=$((n + 1))
  done
  if [ "$n" -eq 0 ]; then
    fail "no template directory under $root"; failures=1
  fi
  CATALOG_FAILURES="$failures"
}

run_gate() {
  resolve_rift
  WORK_DIR="$(mktemp -d)"
  local failures
  run_catalog "$TEMPLATES_DIR" 2525
  failures="$CATALOG_FAILURES"
  if [ "$failures" -ne 0 ]; then
    fail "$failures template(s) failed"
    exit 1
  fi
  log "PASS — every template boots and passes its smoke test"
}

# Write a minimal template to $1. $2 is the smoke check's expected body; $3 an extra stub field.
plant_template() {
  local dir="$1" want="$2" extra="${3:-}"
  mkdir -p "$dir/fixtures"
  echo '{"greeting":"REAL-BODY"}' >"$dir/fixtures/body.json"
  cat >"$dir/imposters.json" <<JSON
{ "imposters": [ { "port": 4999, "protocol": "http", "name": "probe", "recordRequests": true, "stubs": [
  { "predicates": [{ "equals": { "path": "/probe" } }$extra],
    "responses": [{ "is": { "statusCode": 200, "body": "<%- stringify('fixtures/body.json') %>" } }] } ] } ] }
JSON
  echo '{ "name": "planted", "entrypoint": "imposters.json", "requires": { "flags": [] } }' >"$dir/template.json"
  cat >"$dir/smoke.sh" <<SH
#!/usr/bin/env bash
set -euo pipefail
curl -sf -m 5 "\$ADMIN/imposters" >/dev/null || { echo "admin unreachable" >&2; exit 1; }
body="\$(curl -sf -m 5 http://127.0.0.1:4999/probe)" || { echo "probe failed" >&2; exit 1; }
if [[ "\$body" == *"$want"* ]]; then echo ok; else echo "unexpected body: \$body" >&2; exit 1; fi
SH
}

# Prove the gate is not a no-op: a correct template passes; a template whose smoke check fails, one
# that does not boot, and one that needs --allow-injection are each rejected.
self_test() {
  resolve_rift
  WORK_DIR="$(mktemp -d)"
  local good="$WORK_DIR/good" failures

  plant_template "$good/ok" "REAL-BODY"
  run_catalog "$good" 25100
  failures="$CATALOG_FAILURES"
  [ "$failures" -eq 0 ] || { fail "self-test: gate rejected a correct template"; exit 1; }

  local case_dir base=25110
  for case_dir in smoke-fails does-not-boot injects; do
    local root="$WORK_DIR/$case_dir"
    case "$case_dir" in
      smoke-fails)   plant_template "$root/t" "NOPE-MISSING" ;;
      does-not-boot) plant_template "$root/t" "REAL-BODY"; echo '{ "imposters": [' >"$root/t/imposters.json" ;;
      injects)       plant_template "$root/t" "REAL-BODY" ', { "inject": "function (config) { return true; }" }' ;;
    esac
    run_catalog "$root" "$base" 2>/dev/null
    failures="$CATALOG_FAILURES"
    base=$((base + 10))
    if [ "$failures" -eq 0 ]; then
      fail "self-test: gate did NOT catch a broken template ($case_dir) — it is a no-op"; exit 1
    fi
    log "self-test: rejected a broken template ($case_dir)"
  done

  run_catalog "$WORK_DIR/empty" "$base" 2>/dev/null
  failures="$CATALOG_FAILURES"
  [ "$failures" -ne 0 ] || { fail "self-test: an empty catalog passed"; exit 1; }

  log "PASS — self-test: the gate accepts a correct template and rejects broken ones"
}

case "${1:-}" in
  --self-test) self_test ;;
  "") run_gate ;;
  -h | --help) echo "usage: $0 [--self-test]   (RIFT_BIN=/path to reuse a binary)" >&2; exit 64 ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 64 ;;
esac
