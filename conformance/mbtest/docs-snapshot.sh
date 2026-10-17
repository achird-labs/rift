#!/usr/bin/env bash
#
# Save the Mountebank docs pages mbTest's docs tester reads (#1547). Rift serves no HTML (see
# docs/mountebank/differences.md#the-admin-api-serves-json-only), so the pages come from a real
# Mountebank. They must be rendered by a Mountebank on the port the engine under test will use:
# every admin example prints `Host: localhost:<the serving port>`.
#
# The pages also embed the serving Mountebank's working directory (the lookup example writes and
# reads a CSV file there), so Mountebank runs from <workdir>: the mbTest directory the docs tests run
# in, which keeps those files inside the checkout.
#
# usage: docs-snapshot.sh <mb-binary> <out-dir> [port] [workdir]   (port 2525, workdir .)
set -euo pipefail

usage="usage: docs-snapshot.sh <mb-binary> <out-dir> [port] [workdir]"
mb="${1:?$usage}"
out="${2:?$usage}"
port="${3:-2525}"
workdir="${4:-.}"

# The fourteen pages mbTest's web/docsIntegrityTest.js validates (docsScenarios.js lists the same).
pages=(
  /docs/api/mocks /docs/api/proxies /docs/api/injection /docs/api/xpath /docs/api/json
  /docs/protocols/https /docs/protocols/http /docs/api/jsonpath /docs/api/overview
  /docs/gettingStarted /docs/api/predicates /docs/api/behaviors /docs/api/stubs
  /docs/protocols/tcp
)

mkdir -p "$out"
out="$(cd "$out" && pwd)"
(cd "$workdir" && exec "$mb" start --port "$port" --pidfile "$out/mb.pid" --logfile "$out/mb.log" >"$out/mb.out" 2>&1) &
mb_pid=$!
trap 'kill "$mb_pid" 2>/dev/null || true; wait "$mb_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 100); do
  curl -sf -o /dev/null "http://127.0.0.1:$port/" && break
  kill -0 "$mb_pid" 2>/dev/null || { echo "docs-snapshot: mb exited; see $out/mb.out" >&2; exit 1; }
  sleep 0.1
done

for page in "${pages[@]}"; do
  mkdir -p "$out$(dirname "$page")"
  curl -sf -H 'Accept: text/html' -o "$out$page.html" "http://localhost:$port$page" ||
    { echo "docs-snapshot: could not fetch $page" >&2; exit 1; }
done
echo "docs-snapshot: saved ${#pages[@]} pages from Mountebank on port $port into $out"
