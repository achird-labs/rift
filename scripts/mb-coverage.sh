#!/usr/bin/env bash
#
# Mountebank coverage under the differential suite, with a ratchet (issue #1548).
#
# Runs every differential case with Mountebank under NODE_V8_COVERAGE, reports the profile with
# c8 (HTML and json-summary in conformance/differential/coverage/, which is gitignored), and holds
# the summary against conformance/differential/mb-coverage.json: any covered count that dropped
# or rose, any file not classified, or a profile from another node major fails.
#
# Usage:
#   scripts/mb-coverage.sh           # measure and check
#   scripts/mb-coverage.sh --bless   # measure and rewrite the baseline's counts
#
# Needs Mountebank (as for the differential suite: RIFT_MB_BIN, ~/bench-mb, or `mb` on PATH),
# node and npx. RIFT_SERVER_BIN names a prebuilt Rift server; otherwise it is built first.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
harness="$repo_root/conformance/differential"
out="$harness/coverage"

bless=""
case "${1:-}" in
  "") ;;
  --bless) bless=1 ;;
  *) echo "usage: $0 [--bless]" >&2; exit 2 ;;
esac

c8_version="$(node -p "require(process.argv[1]).c8" "$harness/mb-coverage.json")"

rm -rf "$out"
mkdir -p "$out/v8"

if [[ -z "${RIFT_SERVER_BIN:-}" ]]; then
  cargo build -p rift-http-proxy --bin rift-http-proxy
fi

# Every rift-differential test, so this replaces a plain `cargo test -p rift-differential`; the
# ratchet test skips here (no summary yet).
echo "== rift-differential, Mountebank under coverage"
RIFT_DIFFERENTIAL_REQUIRE_MB=1 RIFT_DIFFERENTIAL_MB_COVERAGE="$out/v8" \
  cargo test -p rift-differential -- --nocapture

# The harness records the Mountebank package it profiled; c8's globs resolve against it.
mb_root="$(node -p "require(process.argv[1]).root" "$out/v8/mountebank.json")"

echo "== c8 $c8_version report"
(
  cd "$mb_root"
  npx --yes "c8@$c8_version" report \
    --temp-directory "$out/v8" --reports-dir "$out" \
    --reporter=html --reporter=json-summary --reporter=text-summary \
    --include 'src/**' --exclude 'src/public/**' --all
)

echo "== ratchet against mb-coverage.json"
env RIFT_DIFFERENTIAL_MB_COVERAGE="$out/v8" \
  RIFT_DIFFERENTIAL_MB_COVERAGE_SUMMARY="$out/coverage-summary.json" \
  ${bless:+RIFT_DIFFERENTIAL_MB_COVERAGE_BLESS=1} \
  cargo test -p rift-differential --test differential -- --exact mb_coverage_matches_baseline --nocapture

echo "report: $out/index.html"
