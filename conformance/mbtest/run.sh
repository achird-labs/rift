#!/usr/bin/env bash
#
# Run Mountebank's own functional suite (mbTest, at the tag in MBTEST_REF) against Rift, after the
# same run against Mountebank itself (#1547). See conformance/mbtest/README.md.
#
# usage: conformance/mbtest/run.sh [both|rift|mountebank]     (default: both, Mountebank first)
#
#   RIFT_BIN         rift-http-proxy under test (required unless the engine is mountebank)
#   MB_BIN           Mountebank's mb (default ~/bench-mb/node_modules/.bin/mb); it serves the docs
#                    snapshot and is the baseline engine
#   MBTEST_CHECKOUT  an existing checkout of MBTEST_REF with `npm ci` done in mbTest/ (default:
#                    clone into $RUNNER_TEMP or $TMPDIR and install)
#   OUT_DIR          results (default conformance/mbtest/out)
#
# Three phases per engine, run one at a time: mbTest uses fixed ports (2525-2527, 3000, 4545, ...),
# so two phases or two engines at once would collide.
#   api   one engine on 2525 (--allowInjection --localOnly, as mbTest's tasks/mbtest.js starts it);
#         the http, https, home and imposters-controller tests
#   cli   no engine; every test spawns MB_EXECUTABLE itself (rift-as-mb for Rift)
#   docs  one engine on 2525 and the docs snapshot on 2600; docsScenarios.js, one test per scenario
# Each engine's outcome is held against expected.json by check.py, which sets the exit status.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
engines="${1:-both}"
case "$engines" in
  both) engines="mountebank rift" ;;
  rift | mountebank) ;;
  *) echo "usage: $0 [both|rift|mountebank]" >&2; exit 64 ;;
esac

ref="$(tr -d '[:space:]' <"$here/MBTEST_REF")"
mb_bin="${MB_BIN:-$HOME/bench-mb/node_modules/.bin/mb}"
out="${OUT_DIR:-$here/out}"
engine_port=2525
snapshot_port=2600
metrics_port=19547     # the API/docs engine's /metrics listener (Rift only)
shim_metrics_port=19548
[[ -x "$mb_bin" ]] || { echo "run.sh: no Mountebank at $mb_bin (set MB_BIN)" >&2; exit 2; }
if [[ " $engines " == *" rift "* ]]; then
  [[ -x "${RIFT_BIN:-}" ]] || { echo "run.sh: RIFT_BIN must name the rift-http-proxy binary" >&2; exit 2; }
fi

rm -rf "$out"
mkdir -p "$out"

# The checkout: the pinned tag, mbTest's locked dependencies, and nothing of Mountebank's own
# package (nothing in scope loads it).
if [[ -n "${MBTEST_CHECKOUT:-}" ]]; then
  checkout="$MBTEST_CHECKOUT"
else
  checkout="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mbtest-$ref"
  if [[ ! -d "$checkout/mbTest/node_modules" ]]; then
    rm -rf "$checkout"
    git clone --quiet --depth 1 --branch "$ref" https://github.com/mountebank-testing/mountebank "$checkout"
    (cd "$checkout/mbTest" && npm ci --ignore-scripts --no-audit --no-fund)
  fi
fi
tag="$(git -C "$checkout" describe --tags --exact-match 2>/dev/null || true)"
if [[ "$tag" != "$ref" ]]; then
  echo "run.sh: $checkout is at '${tag:-no tag}', not $ref (MBTEST_REF)" >&2
  exit 2
fi
mbtest="$checkout/mbTest"
mocha=("node" "$mbtest/node_modules/mocha/bin/mocha.js" "--exit")

# Every fixed port the run binds must be free; a leftover engine would answer in place of ours.
port_busy() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
for port in "$engine_port" 2526 2527 "$snapshot_port" 3000 4545 "$metrics_port" "$shim_metrics_port"; do
  if port_busy "$port"; then
    echo "run.sh: port $port is in use; mbTest needs it free" >&2
    exit 2
  fi
done

pids=()
cleanup() {
  local pid
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; done
  rm -f "$mbtest/test.pid"
}
trap cleanup EXIT

wait_for_port() {
  local port="$1" pid="$2" log="$3"
  for _ in $(seq 1 300); do
    port_busy "$port" && return 0
    kill -0 "$pid" 2>/dev/null || { echo "run.sh: the engine exited; see $log" >&2; tail -20 "$log" >&2; return 1; }
    sleep 0.1
  done
  echo "run.sh: nothing listened on $port after 30 s; see $log" >&2
  return 1
}

stop_pid() {
  local pid="$1" kept=()
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  for p in "${pids[@]}"; do [[ "$p" == "$pid" ]] || kept+=("$p"); done
  pids=("${kept[@]+"${kept[@]}"}")
}

# The docs pages, rendered by Mountebank on the port the engine under test will use.
"$here/docs-snapshot.sh" "$mb_bin" "$out/docs-snapshot" "$engine_port" "$mbtest"
python3 "$here/snapserve.py" "$out/docs-snapshot" "$snapshot_port" &
snap_pid=$!
pids+=("$snap_pid")
wait_for_port "$snapshot_port" "$snap_pid" /dev/null

# What the suite contains, independent of any engine: check.py holds every outcome against it, so a
# test that never ran (a hook aborted its file) is a failure, and an expected.json id that upstream
# renamed is caught. js/ and perf/ are not loaded: they require Mountebank's own sources.
(cd "$mbtest" && "${mocha[@]}" --dry-run --reporter json --reporter-option "output=$out/enumeration.json" \
  'api/**/*Test.js' 'cli/**/*Test.js' 'web/**/*Test.js' >/dev/null)
(cd "$mbtest" && MBTEST_DIR="$mbtest" MB_PORT="$snapshot_port" ENGINE_PORT="$engine_port" \
  "${mocha[@]}" --delay --dry-run --reporter json --reporter-option "output=$out/enumeration-docs.json" \
  "$here/docsScenarios.js" >/dev/null)

start_engine() { # engine phase -> pid in $engine_pid
  local engine="$1" phase="$2" log="$out/$1/$2-engine.log"
  if [[ "$engine" == rift ]]; then
    "$RIFT_BIN" --port "$engine_port" --allow-injection --local-only --metrics-port "$metrics_port" \
      >"$log" 2>&1 &
  else
    "$mb_bin" start --port "$engine_port" --allowInjection --localOnly \
      --pidfile "$out/$engine/mb.pid" --logfile "$out/$engine/$phase-mb.log" >"$log" 2>&1 &
  fi
  engine_pid=$!
  pids+=("$engine_pid")
  wait_for_port "$engine_port" "$engine_pid" "$log"
}

run_phase() { # engine phase executable files...
  local engine="$1" phase="$2" executable="$3"
  shift 3
  echo "== $engine: $phase"
  (cd "$mbtest" && MB_PORT="$engine_port" MB_EXECUTABLE="$executable" RIFT_BIN="${RIFT_BIN:-}" \
    SHIM_METRICS_PORT="$shim_metrics_port" "${mocha[@]}" --reporter json \
    --reporter-option "output=$out/$engine/$phase.json" "$@" >"$out/$engine/$phase.out" 2>&1) || true
  rm -f "$mbtest/test.pid"
  [[ -s "$out/$engine/$phase.json" ]] || {
    echo "run.sh: $engine $phase wrote no results; see $out/$engine/$phase.out" >&2
    tail -20 "$out/$engine/$phase.out" >&2
  }
}

status=0
for engine in $engines; do
  mkdir -p "$out/$engine"
  if [[ "$engine" == rift ]]; then executable="$here/rift-as-mb"; else executable="$mb_bin"; fi

  start_engine "$engine" api
  run_phase "$engine" api "$executable" api/homeControllerTest.js api/impostersControllerTest.js \
    'api/http/*Test.js' 'api/https/*Test.js'
  stop_pid "$engine_pid"

  run_phase "$engine" cli "$executable" 'cli/*Test.js'

  start_engine "$engine" docs
  (cd "$mbtest" && MBTEST_DIR="$mbtest" MB_PORT="$snapshot_port" ENGINE_PORT="$engine_port" \
    "${mocha[@]}" --delay --reporter json --reporter-option "output=$out/$engine/docs.json" \
    "$here/docsScenarios.js" >"$out/$engine/docs.out" 2>&1) || true
  stop_pid "$engine_pid"

  python3 "$here/check.py" --engine "$engine" --out "$out" --mbtest-dir "$mbtest" \
    ${GITHUB_STEP_SUMMARY:+--summary "$GITHUB_STEP_SUMMARY"} || status=1
done

exit "$status"
