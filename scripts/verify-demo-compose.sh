#!/usr/bin/env bash
#
# Boot every docs/demo/docker-compose*.yml and prove it still works (issue #669).
#
# These files are the copy-paste starting point for new users, and nothing ran them: the
# docs-examples gate boots the binary against config files without docker, and the only job that
# ran `docker compose up --wait` targeted tests/compatibility/docker-compose.yml (retired in #1341,
# with nothing composed in its place). So a wrong image
# tag, a stale flag, a renamed env var, or a broken healthcheck would reach users unnoticed —
# issue #664 had to hand-edit the healthcheck in five of these and nothing would have caught a miss
# (it did miss one: docker-compose-retry-proxy.yml had no healthcheck at all).
#
# The checks, per demo file:
#
#   1. compose itself accepts the file                  (attributed to the file, so an unreadable
#                                                        demo is not misreported as a missing probe)
#   2. it declares at least one healthchecked service   (else `up --wait` asserts nothing and this
#                                                        gate would pass it vacuously — which is
#                                                        exactly how retry-proxy went unverified)
#   3. every rift image it runs is the image under test (see check_images_under_test)
#   4. `up --wait` brings it up healthy                 (--wait IS the health assertion: it returns
#                                                        non-zero unless every healthchecked service
#                                                        reaches healthy. The self-test plants a
#                                                        probe that never succeeds to prove that,
#                                                        rather than trusting the flag's docs.)
#
# Demo files are DISCOVERED by glob, never listed: a demo nobody remembered to add to a list is
# exactly the one that rots. There is no skip list — all six boot, and a new one is gated by
# default. A demo that cannot boot fails loudly rather than being passed over.
#
# Usage:
#   scripts/verify-demo-compose.sh              # boot every demo and check it
#   scripts/verify-demo-compose.sh --self-test  # prove each check flags a planted violation
#
# Requires: docker, docker compose v2+, jq, and the image under test built locally:
#
#   docker build -t rift-proxy:local -f crates/rift-http-proxy/Dockerfile .
#   docker tag rift-proxy:local zainalpour/rift-proxy:latest   # retry-proxy pins the published tag
#
# The workflow does both. The second is not optional locally either: check_images_under_test makes
# every rift image a demo runs prove it is the build under test, so without it the retry-proxy demo
# fails with "not the image under test".

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

# Overridable so the self-test can point the checks at a mutated copy of the demos.
ROOT="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
DEMO_DIR="${DEMO_DIR:-$ROOT/docs/demo}"

# How long a demo gets to come up healthy. Generous: a cold CI runner pulls upstream images here.
WAIT_TIMEOUT="${WAIT_TIMEOUT:-120}"

# The image under test. Every rift image a demo references must resolve to exactly this one.
RIFT_IMAGE_LOCAL="${RIFT_IMAGE_LOCAL:-rift-proxy:local}"

# How a rift image is recognised among the demos' images (upstream images like http-echo are not
# ours to check). Substring, so both `rift-proxy:local` and `zainalpour/rift-proxy:latest` match.
# A rift image published under some other name would go unchecked rather than wrongly failed; all
# six demos use this name, and widening it to "every image" would fail every upstream a demo needs.
RIFT_IMAGE_MARKER="rift-proxy"

set -uo pipefail

FAILURES=0

# The demo currently booted, so the EXIT trap can tear it down if we are interrupted between `up`
# and `down` (a local Ctrl-C; on an ephemeral runner the VM would take it anyway).
CURRENT_FILE=""
SELF_TEST_TMP=""

fail() {
  echo "  FAIL: $1" >&2
  FAILURES=$((FAILURES + 1))
}

ok() {
  echo "  ok: $1"
}

# Repo-relative path, for messages that would otherwise print an absolute temp path in the
# self-test.
rel() {
  echo "${1#"$ROOT"/}"
}

# Prerequisites a demo needs before it can boot, keyed by file name. A demo absent from this case
# needs none. One that needs a prerequisite but is not listed here fails loudly at boot — it is
# never skipped, which is the whole point of this gate.
prereq_for() {
  case "$1" in
    docker-compose-https.yml) echo "generate-certs.sh" ;;
    docker-compose-intercept.yml) echo "generate-intercept-ca.sh" ;;
    *) echo "" ;;
  esac
}

# Compose project name for a demo file: distinct per file so two demos can never be mistaken for
# each other's orphans. Compose requires [a-z0-9_-].
project_name_for() {
  echo "riftdemo-$(basename "$1" .yml | tr -cd '[:alnum:]-')"
}

# The normalized compose model, or non-zero if compose rejects the file. Read via `compose config`
# rather than a YAML grep: it is the same normalization `up` applies, so this cannot disagree with
# what actually runs. stdout only — compose prints warnings on stderr, and folding those into the
# JSON would break the parse.
compose_config_json() {
  docker compose -f "$1" config --format json 2>/dev/null
}

# Services that declare a healthcheck compose will actually run, from an already-validated config.
#
# A DISABLED healthcheck is not a healthcheck: compose accepts `disable: true` and the equivalent
# `test: ["NONE"]`, both of which are non-null objects that `up --wait` then ignores entirely. To
# check only for presence would let the gate accept "the probe is switched off" as proof of a
# probe — the same vacuous pass that let retry-proxy ship unverified, arrived at by a different
# route, and a likely one: quieting a flaky demo probe is exactly the edit this gate must catch.
#
# A healthcheck with no `test` is required to declare one, rather than trusted. Compose normalizes
# `test: []`, `{}`, `disable: false` and an interval-only block all to "no test key", which means
# "defer to the image's own Dockerfile HEALTHCHECK" — and `up --wait` waits on that only if the
# image actually bakes one in, which this JSON cannot say (it would take a `docker inspect` of every
# image to know). Faced with input it cannot classify, a gate takes the dangerous reading: demand an
# explicit probe and fail loudly. All six demos already declare one, so this costs nothing today,
# and a future demo that leans on an inherited probe gets told to spell it out — which a
# copy-paste-able doc should do anyway.
#
# `test: "NONE"` as a bare STRING is deliberately not excluded: compose sugars it to
# `["CMD-SHELL", "NONE"]`, a probe that really runs (and fails). Only the list form is the keyword.
healthchecked_services() {
  jq -r '.services | to_entries[]
    | select(.value.healthcheck != null)
    | select(.value.healthcheck.disable != true)
    | select(.value.healthcheck.test != null)
    | select(.value.healthcheck.test[0] != "NONE")
    | .key' <<<"$1"
}

compose_down() {
  docker compose -f "$1" -p "$(project_name_for "$1")" down -v --remove-orphans >/dev/null 2>&1
}

cleanup() {
  local rc=$?
  if [ -n "$CURRENT_FILE" ]; then
    compose_down "$CURRENT_FILE"
    CURRENT_FILE=""
  fi
  if [ -n "$SELF_TEST_TMP" ]; then
    rm -rf "$SELF_TEST_TMP"
    SELF_TEST_TMP=""
    # Only ever a second tag on an image the self-test already had, so this unnames it rather than
    # deleting anything.
    docker rmi "$SELF_TEST_WRONG_IMAGE" >/dev/null 2>&1
  fi
  return $rc
}
trap cleanup EXIT

# Every rift image a demo runs must BE the image under test.
#
# docker-compose-retry-proxy.yml pins the PUBLISHED image (zainalpour/rift-proxy:latest) rather
# than rift-proxy:local, which is right for a user copy-pasting it and wrong for CI: left alone,
# this gate would boot whatever is on Docker Hub and report it as coverage of the code under
# review — green on a broken PR, red on an unrelated bad publish. The workflow retags the local
# build onto that pin; this check is what keeps that step honest, because deleting it would
# otherwise degrade the gate silently, which is the failure mode this whole gate exists to stop.
check_images_under_test() {
  local name="$1" config="$2"
  local expected image actual

  expected="$(docker image inspect --format '{{.Id}}' "$RIFT_IMAGE_LOCAL" 2>/dev/null)"
  if [ -z "$expected" ]; then
    fail "$RIFT_IMAGE_LOCAL is not present — build it before running this gate"
    return 1
  fi

  while IFS= read -r image; do
    [ -z "$image" ] && continue
    [[ "$image" != *"$RIFT_IMAGE_MARKER"* ]] && continue
    actual="$(docker image inspect --format '{{.Id}}' "$image" 2>/dev/null)"
    if [ "$actual" != "$expected" ]; then
      fail "$name runs '$image', which is not the image under test ($RIFT_IMAGE_LOCAL) — retag it"
      return 1
    fi
  done < <(jq -r '.services[].image // empty' <<<"$config")

  return 0
}

# Why `up --wait` failed. It is the health assertion, but it does not say WHICH way it failed, and
# "never became healthy" and "never started" send you to different bugs — so ask the containers
# before tearing them down. This is also what gives the self-test a reason to assert that a flaky
# image pull cannot counterfeit.
report_boot_failure() {
  local name="$1" file="$2" project="$3" services="$4"
  local service cid status reported=0

  while IFS= read -r service; do
    [ -z "$service" ] && continue
    # -aq, not -q: a container that started and then exited still has the status worth reporting.
    # head -1 because a scaled service yields several ids, which `docker inspect` would reject —
    # costing the specific message this function exists to produce.
    cid="$(docker compose -f "$file" -p "$project" ps -aq "$service" 2>/dev/null | head -1)"
    [ -z "$cid" ] && continue
    status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$cid" 2>/dev/null)"
    if [ -n "$status" ] && [ "$status" != "healthy" ]; then
      fail "$name service '$service' never became healthy (status: $status)"
      reported=1
    fi
  done <<<"$services"

  if [ "$reported" -eq 0 ]; then
    fail "$name did not come up within ${WAIT_TIMEOUT}s"
  fi
}

# Boot one demo and assert every healthchecked service reaches healthy.
check_demo() {
  local file="$1"
  local name project prereq config services
  name="$(basename "$file")"
  project="$(project_name_for "$file")"

  echo "── $name"

  # Attribute an unreadable file to the file, not to its healthchecks: both fail the gate, but
  # "declares no healthchecked service" would send the next reader hunting for the wrong bug.
  if ! config="$(compose_config_json "$file")" || [ -z "$config" ]; then
    fail "$name is not a valid compose file"
    docker compose -f "$file" config 2>&1 | head -5 | sed 's/^/    | /' >&2
    return
  fi

  services="$(healthchecked_services "$config")"
  if [ -z "$services" ]; then
    fail "$name declares no healthchecked service — 'up --wait' would assert nothing"
    return
  fi

  check_images_under_test "$name" "$config" || return

  prereq="$(prereq_for "$name")"
  if [ -n "$prereq" ]; then
    if ! (cd "$DEMO_DIR" && "./$prereq" >/dev/null 2>&1); then
      fail "$name prerequisite '$prereq' failed"
      return
    fi
    ok "$name prerequisite '$prereq' ran"
  fi

  compose_down "$file"
  CURRENT_FILE="$file"
  if ! docker compose -f "$file" -p "$project" up -d --wait --wait-timeout "$WAIT_TIMEOUT" \
    >/dev/null 2>&1; then
    report_boot_failure "$name" "$file" "$project" "$services"
    docker compose -f "$file" -p "$project" logs --tail 40 >&2 || true
  else
    ok "$name up, all healthchecked services healthy"
  fi
  compose_down "$file"
  CURRENT_FILE=""
}

run_checks() {
  local files=("$DEMO_DIR"/docker-compose*.yml)
  if [ ! -e "${files[0]}" ]; then
    fail "no demo compose files found in $(rel "$DEMO_DIR")"
    return
  fi
  echo "Checking ${#files[@]} demo compose files in $(rel "$DEMO_DIR")"
  local file
  for file in "${files[@]}"; do
    check_demo "$file"
  done
}

# ── self-test ────────────────────────────────────────────────────────────────────────────────
#
# A gate nobody has seen fail is not known to be a gate. Each case plants one violation in a copy
# of the demos and asserts this script rejects it. The image below is the one the retry-proxy demo
# already pulls, so the self-test costs no extra pull; the planted probe path does not exist in any
# image, which makes "never becomes healthy" independent of what shell the image ships.
SELF_TEST_IMAGE="hashicorp/http-echo:latest"

# A tag that matches RIFT_IMAGE_MARKER but is NOT the build under test. The self-test points it at
# a real image it already has, so its case exercises the ID comparison rather than passing on "that
# image isn't here" — a plant that trips a different branch than the one it names proves nothing
# about the branch it names.
SELF_TEST_WRONG_IMAGE="rift-proxy:not-the-build-under-test"

# Counted so the summary can say how much was actually proven. "Self-test passed" over a suite that
# silently stopped planting anything reads exactly like one that still checks five things.
PLANTED=0
CAUGHT=0

# Re-invoke this script as a subprocess against a mutated demo dir — the same way a user runs it,
# and isolated so a planted failure cannot leak into the parent's count.
#
# The case must fail for the REASON under test, not merely fail: a missing image or an absent
# docker daemon also exits non-zero, and accepting that as proof would make this self-test pass
# while testing nothing — the same class of bug (passing for the wrong reason) that issue #669 is
# about. This mirrors `plant`'s `expect` in scripts/verify-image-hardening.sh, for the same reason.
self_test_case() {
  local description="$1" dir="$2" expected_reason="$3"
  local out rc
  PLANTED=$((PLANTED + 1))
  out="$(DEMO_DIR="$dir" WAIT_TIMEOUT=20 "$SELF" 2>&1)"
  rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "  FAIL: $description — the gate accepted it" >&2
    echo "$out" | sed 's/^/    | /' >&2
    return 1
  fi
  if ! grep -qF "$expected_reason" <<<"$out"; then
    echo "  FAIL: $description — rejected, but not for '$expected_reason'" >&2
    echo "$out" | sed 's/^/    | /' >&2
    return 1
  fi
  CAUGHT=$((CAUGHT + 1))
  echo "  ok: $description"
  return 0
}

self_test() {
  local failed=0
  SELF_TEST_TMP="$(mktemp -d)"
  local tmp="$SELF_TEST_TMP"

  echo "Self-test: proving each check rejects a planted violation"

  # 1. A demo with no healthcheck at all: this is the retry-proxy shape that made the gate
  #    vacuous. It must be rejected, not passed over.
  mkdir -p "$tmp/nohealth"
  cat >"$tmp/nohealth/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    command: ["-text", "planted"]
EOF
  self_test_case "a demo with no healthchecked service is rejected" "$tmp/nohealth" \
    "declares no healthchecked service" || failed=1

  # 2. A demo whose probe can never succeed must be rejected. This is what proves `up --wait`
  #    really is a health assertion and not just "the container started" — the entire gate rests
  #    on that, so it is asserted here rather than taken from the flag's documentation.
  mkdir -p "$tmp/unhealthy"
  cat >"$tmp/unhealthy/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    command: ["-text", "planted"]
    healthcheck:
      test: ["CMD", "/probe-that-does-not-exist"]
      interval: 1s
      timeout: 2s
      retries: 2
EOF
  self_test_case "a demo whose probe never succeeds is rejected" "$tmp/unhealthy" \
    "never became healthy" || failed=1

  # 2b/2c. A switched-off probe must not count as a probe. Both of compose's disable spellings get
  #    their own case: they are separate clauses in healthchecked_services, so one could rot while
  #    the other kept the suite green.
  mkdir -p "$tmp/disabled"
  cat >"$tmp/disabled/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    command: ["-text", "planted"]
    healthcheck:
      disable: true
EOF
  self_test_case "a demo whose healthcheck is disabled is rejected" "$tmp/disabled" \
    "declares no healthchecked service" || failed=1

  mkdir -p "$tmp/probe-none"
  cat >"$tmp/probe-none/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    command: ["-text", "planted"]
    healthcheck:
      test: ["NONE"]
EOF
  self_test_case "a demo whose healthcheck test is NONE is rejected" "$tmp/probe-none" \
    "declares no healthchecked service" || failed=1

  # 2d. A healthcheck block with no probe of its own must be rejected, not trusted to inherit one
  #    from the image (compose only waits on an inherited probe if the image has one, which the
  #    config cannot tell us — so this is the fail-closed reading).
  mkdir -p "$tmp/no-test"
  cat >"$tmp/no-test/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    command: ["-text", "planted"]
    healthcheck:
      interval: 5s
EOF
  self_test_case "a demo whose healthcheck declares no probe is rejected" "$tmp/no-test" \
    "declares no healthchecked service" || failed=1

  # 3. A file compose itself rejects must be reported as an unreadable FILE, not misattributed to
  #    its healthchecks — a gate that fails for a plausible-but-wrong reason costs the next reader
  #    the debugging time this gate was meant to save.
  mkdir -p "$tmp/invalid"
  cat >"$tmp/invalid/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_IMAGE
    ports: "this is not a port list"
EOF
  self_test_case "a compose file compose itself rejects is reported as invalid" "$tmp/invalid" \
    "is not a valid compose file" || failed=1

  # 4. A demo running a rift image that is not the build under test must be rejected — otherwise
  #    dropping the workflow's retag would silently downgrade this gate to testing Docker Hub.
  mkdir -p "$tmp/wrongimage"
  docker tag "$SELF_TEST_IMAGE" "$SELF_TEST_WRONG_IMAGE" >/dev/null 2>&1
  cat >"$tmp/wrongimage/docker-compose-planted.yml" <<EOF
services:
  planted:
    image: $SELF_TEST_WRONG_IMAGE
    healthcheck:
      test: ["CMD", "rift", "healthcheck"]
      interval: 1s
      timeout: 2s
      retries: 2
EOF
  self_test_case "a demo not running the image under test is rejected" "$tmp/wrongimage" \
    "not the image under test" || failed=1

  # 5. An empty demo dir must be rejected rather than reported as "all clean" — a glob that
  #    silently matches nothing is how this gate would rot into a no-op.
  mkdir -p "$tmp/empty"
  self_test_case "a demo dir with no compose files is rejected" "$tmp/empty" \
    "no demo compose files found" || failed=1

  if [ "$failed" -ne 0 ]; then
    echo "Self-test FAILED — the gate does not reject what it claims to ($CAUGHT/$PLANTED caught)." >&2
    return 1
  fi
  echo "Self-test passed: $CAUGHT/$PLANTED planted violations caught."
  return 0
}

require_tools() {
  local tool
  for tool in docker jq; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "error: '$tool' is required" >&2
      exit 2
    fi
  done
  # `command -v docker` says nothing about the v2 compose plugin, whose absence would otherwise
  # surface as every demo failing for an unrelated-looking reason.
  if ! docker compose version >/dev/null 2>&1; then
    echo "error: 'docker compose' (v2 plugin) is required" >&2
    exit 2
  fi
}

require_tools

# An unrecognised argument must not quietly run the full gate and exit 0: a typo'd --self-test in
# the workflow would then run the plain gate, pass, and retire the self-test with nobody told.
case "${1:-}" in
  --self-test)
    self_test || exit 1
    exit 0
    ;;
  "") ;;
  *)
    echo "usage: $0 [--self-test]" >&2
    exit 64
    ;;
esac

run_checks

echo
if [ "$FAILURES" -ne 0 ]; then
  echo "FAILED: $FAILURES check(s) failed." >&2
  exit 1
fi
echo "All demo compose files boot and report healthy."
