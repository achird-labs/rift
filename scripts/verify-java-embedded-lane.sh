#!/usr/bin/env bash
#
# Java lane guard for `sdk-matrix.yml` (issues #920, #924, #927).
#
# The matrix replays each released SDK's own conformance suite against the *newest* engine release.
# For rift-java that suite is `CorpusReplayIT`, which aborts its fixtures — reporting them as
# surefire `skipped` rather than failing — whenever the engine or corpus is unusable. A green maven
# build therefore cannot prove the embedded replay ran (#920), and the count alone cannot prove it
# ran over the *embedded* transport rather than spawning a binary (#924).
#
# #924's transport proof grepped the surefire XML for the `[EMBEDDED]` tag that `CorpusReplayIT`
# puts in its dynamic-test display names. That is unsatisfiable on every run, correct or broken:
# maven-surefire's default XML reporter does not record JUnit 5 dynamic-test display names, so a
# test displayed as `01 · Basic REST API [EMBEDDED]` is written as `<testcase name="replay()[1]">`.
# The tag never reaches the file the guard reads, so the java lane was red against a healthy SDK
# (#927).
#
# The transport is proven by negative control instead. `ConformanceTransport.resolve()` hard-errors
# on an unrecognized `$CONFORMANCE_TRANSPORT`, so a second invocation carrying a bogus value MUST
# fail. If it passes, the SDK tag is ignoring the variable (or self-skipping before transport
# selection, the original #920 condition) and the green run above proves nothing about which
# transport it used.
#
# The canary speaks only for the value it sets itself, so it cannot vouch for the one the *step*
# supplied — hence the separate assertion that the ambient `$CONFORMANCE_TRANSPORT` is `EMBEDDED`.
# Together the three checks chain: the step asks for EMBEDDED, the SDK demonstrably reads the
# variable, and the report shows replays actually ran ⇒ those replays ran embedded.
#
# That inference only holds if the canary failed *in the suite*. A canary that dies in maven's own
# plumbing — unresolved dependency, bad goal, missing module — is equally non-zero and would make
# the guard vacuously green, which is the failure mode this whole file exists to prevent. So the
# canary must also leave a regenerated report recording errors or failures.
#
# Deliberately not fixed here: teaching the SDK's own pom to emit phrased test-case names, which
# would put the transport tag in the XML and allow a direct assertion. That needs a new rift-java
# release, and this gate replays *released* tags — including old ones — so it must work against
# v0.2.3 as it already exists.
#
# Usage:
#   scripts/verify-java-embedded-lane.sh [--module-dir DIR]   # run from the SDK checkout root
#   scripts/verify-java-embedded-lane.sh --self-test          # prove the guard isn't a no-op
#
set -euo pipefail

# Indirection so --self-test can substitute a stub for the real build tool.
MVN="${MVN:-./mvnw}"

# An unrecognized transport value. Any string `resolve()` cannot map is equivalent; this one is
# self-describing in a CI log.
CANARY_TRANSPORT='__DRIFT_CANARY__'

usage() {
  sed -n '3,41p' "${BASH_SOURCE[0]}" >&2
  exit 2
}

fail() {
  printf '::error::%s\n' "$1" >&2
  return 1
}

# The `|| true` is load-bearing under `set -euo pipefail`: with no `target/` directory `find` exits
# 1, and `head` closing early on a multi-match can hand back SIGPIPE (141). Either would abort the
# caller's bare assignment before its `::error::` annotation is emitted, turning a diagnosed failure
# into a silent red — which, given #927's own lesson, is the most expensive way for this to break.
find_report() {
  # `sort` before `head`: `find`'s order is filesystem-dependent, and a lane that ever holds two
  # matching reports must read the same one on every runner rather than whichever came first.
  find "$1/target" -name 'TEST-*CorpusReplayIT.xml' -type f 2>/dev/null | sort | head -1 || true
}

# Reads a numeric attribute off the report's <testsuite> element. `-m1` takes that element, which
# surefire always writes before any <testcase>. Emits 0 for a missing file or attribute so callers
# can compare without guarding every substitution — and so an unreadable report reads as "nothing
# ran", which is the fail-closed direction for every caller below.
attr() {
  [ -f "$1" ] || { printf '0'; return 0; }
  local value
  value="$(grep -m1 -o "$2=\"[0-9]*\"" "$1" | tr -dc '0-9' || true)"
  printf '%s' "${value:-0}"
}

verify() {
  local module="$1" report tests skipped errors failures

  # The main verify ran under the *step's* $CONFORMANCE_TRANSPORT; the canary below overrides it
  # per-invocation, so no other check here would notice if the step stopped setting it at all. An
  # SDK tag that maps an absent value to a default rather than erroring would then replay over that
  # default while every check below still passed — the #924 defect relocated to the one input the
  # canary cannot speak for. Assert the ambient value before trusting anything downstream of it.
  [ "${CONFORMANCE_TRANSPORT:-}" = 'EMBEDDED' ] \
    || fail "the java lane must run with CONFORMANCE_TRANSPORT=EMBEDDED, got '${CONFORMANCE_TRANSPORT:-<unset>}' — whatever replayed above was not the embedded transport" \
    || return 1

  report="$(find_report "$module")"
  [ -n "$report" ] \
    || fail "no CorpusReplayIT report — the embedded conformance suite did not run" || return 1

  tests="$(attr "$report" tests)"
  skipped="$(attr "$report" skipped)"
  [ "$tests" -gt "$skipped" ] \
    || fail "CorpusReplayIT ran $tests tests with $skipped skipped — the embedded replay was silently skipped" \
    || return 1

  # Negative control. Mirrors the main invocation's reactor shape — `-pl <module> -am` — rather than
  # invoking `surefire:test` on the module alone. `-pl` without `-am` prunes the reactor to one
  # module, so the SDK's sibling artifacts must resolve from a repository; at v0.2.3 they are
  # `-SNAPSHOT` versions that the preceding `verify` never installed (it stops a phase short of
  # `install`) and that no declared repository serves. That canary dies at dependency resolution,
  # never reaches the suite, and takes the "failed outside the suite" branch below — a lane red
  # against a healthy SDK, which is #924's failure mode rebuilt. `-Dtest` narrows surefire to the
  # replay class; `failIfNoSpecifiedTests=false` stops the upstream modules, which have no such
  # class, from failing on that filter.
  if CONFORMANCE_TRANSPORT="$CANARY_TRANSPORT" \
     "$MVN" -B -ntp -pl "$module" -am verify \
       -Dtest=CorpusReplayIT -Dsurefire.failIfNoSpecifiedTests=false; then
    fail "transport canary passed — \$CONFORMANCE_TRANSPORT is not honoured by this SDK tag (or the replay self-skipped); the embedded result above proves nothing" || return 1
  fi

  # The canary must have failed inside the suite, not in maven's plumbing. A canary that never
  # reached the tests leaves the main run's report untouched, so its errors/failures stay at 0.
  report="$(find_report "$module")"
  errors="$(attr "$report" errors)"
  failures="$(attr "$report" failures)"
  [ "$((errors + failures))" -gt 0 ] \
    || fail "transport canary failed but its CorpusReplayIT report records no error — it failed outside the suite (maven plumbing), so it proves nothing about transport selection" \
    || return 1

  # Attribution. An in-suite error only says *something* broke; a flaky fixture or a port collision
  # with the replay that just finished would satisfy the count while the transport selector was
  # never consulted. `resolve()` quotes the offending value back —
  #   unknown CONFORMANCE_TRANSPORT '<value>' (expected SPAWN or EMBEDDED)
  # — so the sentinel's presence in the report names the rejection instead of inferring it.
  # Unlike #924's grep this asserts text the default reporter demonstrably carries: an exception
  # message, which surefire writes into the report, not a JUnit 5 dynamic display name, which it
  # does not.
  grep -qF "$CANARY_TRANSPORT" "$report" \
    || fail "transport canary failed inside the suite, but its report never names '$CANARY_TRANSPORT' — something other than the transport selector rejected it, so the embedded result is unproven" \
    || return 1

  printf 'java embedded lane: OK — %s replays ran (%s skipped), transport canary rejected as expected\n' \
    "$tests" "$skipped"
}

# --- self-test -----------------------------------------------------------------------------------
# Drives the real `verify` against a stub build tool. Every branch here is a way the java lane can
# report success while having proven nothing — which is precisely what #924's grep did and what
# #920 set out to catch. The workflow itself runs this logic only against live SDK releases, so
# this is the only place its polarity is ever checked.
self_test() {
  local tmp status=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat >"$tmp/mvn" <<'STUB'
#!/usr/bin/env bash
# Stub mvnw: records the transport it was handed, optionally rewrites the surefire report, and
# exits with the code the case under test needs.
set -euo pipefail
printf '%s\n' "${CONFORMANCE_TRANSPORT:-<unset>}" >>"$STUB_STATE/transports"
if [ -n "${STUB_REPORT_ERRORS:-}" ]; then
  mkdir -p "$STUB_STATE/module/target/surefire-reports"
  # Mirrors what surefire writes for a @BeforeAll failure: the exception message, verbatim, in the
  # testcase's <error>. The default is resolve()'s real message shape at rift-java v0.2.3.
  {
    printf '<testsuite name="io.github.achirdlabs.rift.conformance.CorpusReplayIT" tests="1" errors="%s" skipped="0" failures="0">\n' \
      "$STUB_REPORT_ERRORS"
    printf '  <testcase name="replay()"><error message="%s"/></testcase>\n' \
      "${STUB_ERROR_MESSAGE:-unknown CONFORMANCE_TRANSPORT '${CONFORMANCE_TRANSPORT:-}' (expected SPAWN or EMBEDDED)}"
    printf '</testsuite>\n'
  } >"$STUB_STATE/module/target/surefire-reports/TEST-io.rift.conformance.CorpusReplayIT.xml"
fi
exit "${STUB_EXIT:-1}"
STUB
  chmod +x "$tmp/mvn"

  export STUB_STATE="$tmp"
  local module="$tmp/module"

  # What the workflow step supplies. Cases G/H below override it to prove the guard notices when
  # the step does not.
  export CONFORMANCE_TRANSPORT='EMBEDDED'

  # Writes the report the main `./mvnw verify` would have left behind. Same file name as the
  # stub above rewrites: surefire names the report after the test class, so the main run and the
  # canary write ONE file, and the guard reads that one. Two differently named files here would
  # leave `find_report` reading whichever `find` happened to list first — green on one filesystem
  # and red on another, which is exactly what happened on the CI runner.
  write_report() {
    mkdir -p "$module/target/surefire-reports"
    printf '<testsuite name="io.rift.conformance.CorpusReplayIT" tests="%s" errors="0" skipped="%s" failures="0"/>\n' \
      "$1" "$2" >"$module/target/surefire-reports/TEST-io.rift.conformance.CorpusReplayIT.xml"
  }

  # Runs `verify` under one stub configuration; asserts on exit polarity and diagnostic text.
  case_is() {
    local name="$1" want="$2" want_msg="$3" out rc=0
    out="$(MVN="$tmp/mvn" verify "$module" 2>&1)" || rc=$?
    if [ "$want" = pass ] && [ "$rc" -ne 0 ]; then
      printf 'FAIL: %s — expected green, got exit %s:\n%s\n' "$name" "$rc" "$out" >&2
      status=1
    elif [ "$want" = fail ] && [ "$rc" -eq 0 ]; then
      printf 'FAIL: %s — expected red, got green:\n%s\n' "$name" "$out" >&2
      status=1
    elif [ -n "$want_msg" ] && ! printf '%s' "$out" | grep -qF "$want_msg"; then
      printf 'FAIL: %s — diagnostic did not mention %q:\n%s\n' "$name" "$want_msg" "$out" >&2
      status=1
    fi
  }

  # A — the suite never ran at all: no report to read.
  rm -rf "$module"; mkdir -p "$module"
  : >"$tmp/transports"
  STUB_EXIT=1 case_is 'A missing report' fail 'the embedded conformance suite did not run'

  # B — the replay self-skipped every fixture (#920): green maven, all-skipped report.
  rm -rf "$module"; write_report 2 2
  STUB_EXIT=1 case_is 'B all fixtures skipped' fail 'silently skipped'

  # C — the SDK tag ignores $CONFORMANCE_TRANSPORT, so the bogus value is accepted (#924's target).
  rm -rf "$module"; write_report 15 2
  STUB_EXIT=0 case_is 'C canary accepted' fail 'transport canary passed'

  # D — the canary died in maven's plumbing and never reached the suite: red, but it proves nothing.
  rm -rf "$module"; write_report 15 2
  STUB_EXIT=1 case_is 'D canary failed outside the suite' fail 'failed outside the suite'

  # E — the healthy path: replays ran, canary rejected by the suite.
  rm -rf "$module"; write_report 15 2
  : >"$tmp/transports"
  STUB_EXIT=1 STUB_REPORT_ERRORS=1 case_is 'E healthy lane' pass ''

  # J — the canary failed inside the suite for a reason that has nothing to do with the transport
  # (here a port collision with the replay that just finished). The count check cannot tell this
  # from a real rejection; only the sentinel can, and without it the guard would report the embedded
  # transport proven on the strength of an unrelated flake.
  rm -rf "$module"; write_report 15 2
  STUB_EXIT=1 STUB_REPORT_ERRORS=1 \
    STUB_ERROR_MESSAGE='java.net.BindException: Address already in use' \
    case_is 'J canary failed for an unrelated reason' fail "never names '$CANARY_TRANSPORT'"

  # G — the step stopped setting $CONFORMANCE_TRANSPORT (the regression #927's acceptance criterion
  # names). The canary sets its own value, so nothing else here would notice; an SDK tag defaulting
  # an absent value to SPAWN would otherwise replay non-embedded and still be reported OK.
  rm -rf "$module"; write_report 15 2
  CONFORMANCE_TRANSPORT='' STUB_EXIT=1 STUB_REPORT_ERRORS=1 \
    case_is 'G ambient transport unset' fail 'must run with CONFORMANCE_TRANSPORT=EMBEDDED'

  # H — the step sets a real but wrong transport. Same hole, but reachable without anyone deleting
  # a line: the lane would be replaying spawn while reporting on the embedded lane's behalf.
  rm -rf "$module"; write_report 15 2
  CONFORMANCE_TRANSPORT='SPAWN' STUB_EXIT=1 STUB_REPORT_ERRORS=1 \
    case_is 'H ambient transport is SPAWN' fail 'must run with CONFORMANCE_TRANSPORT=EMBEDDED'

  # I — the real entry path, as a subprocess. Every case above calls `verify` in-process from a
  # command substitution, where bash suppresses `set -e` for the whole function body; production
  # does not, so a pipeline that exits non-zero mid-function aborts before its `::error::` is
  # printed and the lane goes red with no diagnostic at all. That divergence is invisible to an
  # in-process case by construction — it can only be caught by running the script the way CI does.
  rm -rf "$module"; mkdir -p "$module"
  local e2e_out e2e_rc=0
  e2e_out="$(CONFORMANCE_TRANSPORT='EMBEDDED' MVN="$tmp/mvn" STUB_EXIT=1 \
    bash "${BASH_SOURCE[0]}" --module-dir "$module" 2>&1)" || e2e_rc=$?
  if [ "$e2e_rc" -eq 0 ]; then
    printf 'FAIL: I end-to-end missing report — expected red, got green\n' >&2
    status=1
  elif ! printf '%s' "$e2e_out" | grep -qF 'the embedded conformance suite did not run'; then
    printf 'FAIL: I end-to-end missing report — red with no ::error:: diagnostic (got %q)\n' \
      "$e2e_out" >&2
    status=1
  fi

  # F — the canary is only a control if it carried a value `resolve()` cannot map. Asserted against
  # the real transport names rather than against $CANARY_TRANSPORT, which would make this vacuous:
  # a canary retargeted at a legitimate transport would be accepted, so its failure — and with it
  # every case above — would say nothing about whether the variable is read. The names mirror
  # `ConformanceTransport`'s cases in rift-java; a transport added there weakens this check until
  # it is added here, which is the cost of the gate replaying released tags it cannot import from.
  local canary_value
  canary_value="$(tail -1 "$tmp/transports")"
  case "$canary_value" in
    EMBEDDED | SPAWN | '<unset>' | '')
      printf 'FAIL: F canary transport — the canary carried %q, which the SDK can resolve; a negative control must be unrecognizable\n' \
        "$canary_value" >&2
      status=1
      ;;
  esac

  if [ "$status" -eq 0 ]; then
    printf 'self-test: OK — a missing or all-skipped report, a wrong/absent ambient transport, an accepted canary, and a canary that failed outside the suite or for an unrelated reason are all red, with the diagnostic surviving the real entry path; a healthy lane is green\n'
  fi
  return "$status"
}

module_dir='rift-java-conformance'
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) self_test; exit $? ;;
    --module-dir) module_dir="${2:?--module-dir needs a value}"; shift 2 ;;
    -h|--help) usage ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage ;;
  esac
done

verify "$module_dir"
