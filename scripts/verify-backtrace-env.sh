#!/usr/bin/env bash
#
# Backtrace-cost gate (issue #1021).
#
# `RUST_BACKTRACE=1` in a test workflow is not free, and its cost is not paid by the panicking
# thread. std's panic hook takes a **process-global** mutex for the whole backtrace print,
# symbolisation included, and `std::backtrace::Backtrace::capture()` takes that same lock —
# which `anyhow` calls on **every** error construction whenever a backtrace env var is enabled.
# So one deliberately-panicking test freezes every other test in the binary that builds an
# `anyhow::Error`, for as long as it takes to resolve a multi-hundred-megabyte debug binary's
# DWARF. Measured on CI: a 3.77 s stall, ended by the panicker, with ten tests completing in the
# 2.8 ms after it.
#
# That is how #1021 presented: two different timing tests failing on two consecutive runs of a
# branch whose diff (a Dockerfile digest bump) could not affect them. The tests were not flaky —
# they were the only ones in the binary *measuring* the freeze.
#
# `RUST_LIB_BACKTRACE=0` fixes it: std checks it first for `capture()`, so anyhow stops touching
# the lock, while `RUST_BACKTRACE=1` keeps panics' own backtraces intact. Verified on the
# toolchain rather than assumed — `Backtrace::capture().status()` goes `Captured` -> `Disabled`,
# and a panic's printed backtrace is byte-identical either way.
#
# The trade-off, stated plainly because it is real: this also removes anyhow's backtrace from CI
# *failure output*, not just from the hot path. A test that fails by panicking — `assert!`,
# `unwrap`, an `expect` — still prints a full backtrace, which is the overwhelming majority. A
# test that fails by propagating an `anyhow::Error` now shows the error chain without a captured
# backtrace. If you are debugging exactly that, re-run locally with `RUST_LIB_BACKTRACE=1`.
#
# The invariant is keyed on the hazard, not on a filename: a workflow only has this problem if it
# both enables backtraces AND runs tests. `release.yml` enables them and runs no tests, so it is
# correctly exempt — and would be caught automatically if it ever started running them.
#
# Three things this gate deliberately does NOT do naively:
#
#   * `RUST_LIB_BACKTRACE=1` counts as *enabling*, not as a mitigation. std checks that variable
#     FIRST for `capture()` — which is the whole premise of this fix — so setting it to 1 makes
#     anyhow capture no matter what `RUST_BACKTRACE` says. A gate that only looked at
#     `RUST_BACKTRACE` would wave through the identical hazard spelled the other way.
#   * It counts, rather than merely detecting. A workflow can enable backtraces more than once —
#     a job- or matrix-scoped `env:` overriding the workflow-level one — and a file-wide "is
#     RUST_LIB_BACKTRACE present anywhere?" check would pass such a file while one of its jobs
#     carries the full hazard. Requiring at least as many mitigations as enablings catches the
#     job-scoped override that a presence check waves through.
#   * It reads the *value* of `[profile.test] debug`, not just the key. `debug = true` is exactly
#     what the profile already inherits from `[profile.dev]`, so a key-presence check would call
#     the unmitigated state "ok" — a classifier reporting the dangerous class as safe.
#
# KNOWN LIMITATION, stated rather than papered over: this is a line-oriented check, so it cannot
# model GitHub's one-way workflow->job `env:` inheritance. A file whose workflow-level `env:`
# enables backtraces once, and which mitigates once inside a job that does NOT run tests, has
# equal counts and passes here while a sibling test job is still exposed. Catching that needs a
# real YAML parse of the effective env per job; the counting rule above is what a line-oriented
# gate can honestly promise. The invariant is keyed on the FILE, not on the job.
#
# Usage:
#   scripts/verify-backtrace-env.sh              # check the workflows (exit 1 on any gap)
#   scripts/verify-backtrace-env.sh --self-test  # prove the checker flags planted gaps
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# EITHER backtrace variable set to anything but 0 — single- or double-quoted, or bare. Anchored to
# the start of the line so a commented-out key, or a `RUST_BACKTRACE=1` inside a `run:` block, does
# not match. `RUST_LIB_BACKTRACE: 1` belongs here, not in the mitigation pattern: std reads it
# first, so it enables capture on its own.
ENABLE_RE='^[[:space:]]*RUST_(LIB_)?BACKTRACE:[[:space:]]*["'\'']?[^0"'\''[:space:]]'
# `RUST_LIB_BACKTRACE: 0`, tolerating either quote style and a YAML trailing comment.
MITIGATE_RE='^[[:space:]]*RUST_LIB_BACKTRACE:[[:space:]]*["'\'']?0["'\'']?[[:space:]]*(#.*)?$'
# Anything that runs the test suite. Deliberately wider than `cargo test`: a coverage or
# feature-matrix job (`cargo llvm-cov`, `cargo hack test`, `cargo +nightly test`, `cargo-nextest
# run`) builds the same test binaries and carries the same hazard, and those are exactly the jobs
# somebody adds later. A `make test` wrapper counts too.
TESTS_RE='cargo([[:space:]]+\+[^[:space:]]+)?[[:space:]]+(test|nextest|hack|llvm-cov)|cargo-nextest|make[[:space:]]+test'

# Count matching lines. grep exits 0 (match), 1 (no match) or >=2 (could not read the file); the
# last must never be mistaken for "no match", or an unreadable workflow reads as compliant.
count_matches() {
  local pattern="$1" file="$2" out rc
  set +e
  out="$(grep -Ec "$pattern" "$file")"
  rc=$?
  set -e
  if [ "$rc" -ge 2 ]; then
    echo "  ERROR:  $(basename "$file") could not be read — refusing to report it as compliant." >&2
    return 2
  fi
  printf '%s' "$out"
}

check_workflows() {
  local dir="$1" gaps=0 checked=0 exempt=0
  local wf enab mit tests
  for wf in "$dir"/*.yml "$dir"/*.yaml; do
    [ -e "$wf" ] || continue

    if ! enab="$(count_matches "$ENABLE_RE" "$wf")"; then gaps=$((gaps + 1)); continue; fi
    [ "$enab" -gt 0 ] || continue

    if ! tests="$(count_matches "$TESTS_RE" "$wf")"; then gaps=$((gaps + 1)); continue; fi
    if [ "$tests" -eq 0 ]; then
      echo "  exempt: $(basename "$wf") — enables backtraces but runs no tests"
      exempt=$((exempt + 1))
      continue
    fi

    checked=$((checked + 1))
    if ! mit="$(count_matches "$MITIGATE_RE" "$wf")"; then gaps=$((gaps + 1)); continue; fi

    if [ "$mit" -ge "$enab" ]; then
      echo "  ok:     $(basename "$wf") — $enab RUST_BACKTRACE enabling(s), $mit RUST_LIB_BACKTRACE: 0"
    else
      echo "  GAP:    $(basename "$wf") — runs tests and enables RUST_BACKTRACE $enab time(s) but" >&2
      echo "          sets RUST_LIB_BACKTRACE: 0 only $mit time(s). Every anyhow::Error in a test" >&2
      echo "          binary under an unmitigated scope contends for the panic-hook lock (#1021)." >&2
      echo "          A job- or matrix-scoped env: that re-enables backtraces needs its own." >&2
      gaps=$((gaps + 1))
    fi
  done

  # Deliberately `checked`, NOT `checked && exempt`: an exempt file (backtraces on, no tests)
  # proves nothing about the invariant. If ci.yml ever stopped enabling backtraces, release.yml
  # alone would keep `exempt` at 1 and this guard would sit silent while the gate verified
  # nothing at all — a green check that means "I did not look".
  if [ "$checked" -eq 0 ]; then
    echo "  GAP:    no workflow under $dir both enables backtraces AND runs tests, so this gate" >&2
    echo "          verified nothing — which is indistinguishable from passing. Either a test" >&2
    echo "          workflow lost its backtrace env, or this gate has outlived its reason to" >&2
    echo "          exist and should be deleted deliberately rather than left green." >&2
    return 1
  fi
  [ "$gaps" -eq 0 ]
}

# `[profile.test] debug` shrinks the DWARF the panic hook symbolises while holding the lock. The
# VALUE is what matters: `true`/`2`/`"full"` is the full-DWARF state the profile already inherits
# from `[profile.dev]`, so accepting it would be accepting no mitigation at all.
check_profile() {
  local manifest="$1" value
  value="$(awk '
    /^\[profile\.test\]/ { f = 1; next }
    /^\[/               { f = 0 }
    f && /^[[:space:]]*debug[[:space:]]*=/ {
      sub(/^[[:space:]]*debug[[:space:]]*=[[:space:]]*/, "")
      sub(/[[:space:]]*#.*$/, "")
      gsub(/[[:space:]]/, "")
      print
      exit
    }' "$manifest")"

  if [ -z "$value" ]; then
    echo "  GAP:    $(basename "$manifest") — no [profile.test] debug setting, so test binaries" >&2
    echo "          inherit [profile.dev] debug = true and carry full DWARF (issue #1021)." >&2
    return 1
  fi

  case "$value" in
    true|2|'"full"')
      echo "  GAP:    $(basename "$manifest") — [profile.test] debug = $value is full debug info," >&2
      echo "          identical to what it already inherits from [profile.dev]. That is the" >&2
      echo "          unmitigated state issue #1021 exists to fix, not a mitigation." >&2
      return 1
      ;;
    *)
      echo "  ok:     $(basename "$manifest") — [profile.test] debug = $value"
      ;;
  esac
}

# The self-test's job is to prove this checker is not a no-op. Every planted case below MUST be
# rejected, and every corrected case MUST be accepted — a checker that always fails is as useless
# as one that always passes.
self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/workflows"

  expect_reject() {
    if check_workflows "$tmp/workflows" >/dev/null 2>&1; then
      echo "self-test FAILED: the checker accepted $1" >&2
      return 1
    fi
  }
  expect_accept() {
    if ! check_workflows "$tmp/workflows" >/dev/null 2>&1; then
      echo "self-test FAILED: the checker rejected $1" >&2
      return 1
    fi
  }

  expect_profile_reject() {
    if check_profile "$tmp/Cargo.toml" >/dev/null 2>&1; then
      echo "self-test FAILED: the checker accepted $1" >&2
      return 1
    fi
  }
  expect_profile_accept() {
    if ! check_profile "$tmp/Cargo.toml" >/dev/null 2>&1; then
      echo "self-test FAILED: the checker rejected $1" >&2
      return 1
    fi
  }

  # A compliant workflow that runs tests, dropped alongside a planted gap so `checked` is never 0.
  # Without it, a mutation that stops the gate from RECOGNISING the planted file makes the file
  # vanish from the check entirely, `checked` falls to 0, and the anti-no-op guard rejects — so the
  # case passes for the wrong reason and pins nothing. Mutation testing caught exactly that: with
  # the enable-regex and test-detection cases unisolated, reverting either fix left the self-test
  # green. Cases that are ABOUT the anti-no-op guard deliberately omit this.
  write_baseline() {
    cat > "$tmp/workflows/baseline-ok.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
  RUST_LIB_BACKTRACE: 0
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  }
  drop_baseline() { rm -f "$tmp/workflows/baseline-ok.yml"; }

  # Cases 1-7 keep a compliant workflow beside the planted one, so a rejection can only come from
  # the planted gap and never from the anti-no-op guard.
  write_baseline

  # 1. Enables backtraces, runs tests, no mitigation at all.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_reject "a workflow with no RUST_LIB_BACKTRACE"

  # 2. The same file, mitigated — must now pass.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
  RUST_LIB_BACKTRACE: 0
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_accept "a correctly-mitigated workflow"

  # 3. A YAML trailing comment on the mitigation line is still a mitigation.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
  RUST_LIB_BACKTRACE: 0 # see scripts/verify-backtrace-env.sh
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_accept "a mitigation carrying a trailing comment"

  # 4. The job-scoped override a file-wide presence check would wave through: job `b` re-enables
  #    backtraces for itself and runs tests, with no mitigation of its own.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
jobs:
  a:
    env:
      RUST_BACKTRACE: 1
      RUST_LIB_BACKTRACE: 0
    steps:
      - run: cargo test -p a
  b:
    env:
      RUST_BACKTRACE: 1
    steps:
      - run: cargo test -p b
YAML
  expect_reject "a job-scoped RUST_BACKTRACE override with no mitigation"

  # 5. `RUST_LIB_BACKTRACE: 1` on its own is the same hazard spelled differently — std reads it
  #    first, so anyhow captures regardless of RUST_BACKTRACE. It must count as enabling.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_LIB_BACKTRACE: 1
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_reject "RUST_LIB_BACKTRACE: 1 alone, which enables capture on its own"

  # 6. A single-quoted mitigation is still a mitigation.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: '1'
  RUST_LIB_BACKTRACE: '0'
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_accept "a single-quoted mitigation"

  # 7. A coverage job builds the same test binaries and carries the same hazard, so the
  #    test-detection must not be narrowed to the literal string `cargo test`.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
jobs:
  coverage:
    steps:
      - run: cargo llvm-cov --workspace
YAML
  expect_reject "an unmitigated coverage job (cargo llvm-cov)"

  # Cases 8-9 are ABOUT the anti-no-op guard, so the compliant baseline must go: with it present
  # `checked` would be 1 and the guard could never fire.
  drop_baseline

  # 8. Nothing enables backtraces at all — the gate must say it is checking nothing rather than
  #    report a vacuous pass.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
jobs:
  test:
    steps:
      - run: cargo test --all
YAML
  expect_reject "a workflow set in which nothing enables backtraces"

  # 9. Only an EXEMPT file (backtraces on, no tests). It proves nothing about the invariant, so
  #    the anti-no-op guard must still fire rather than let `exempt` stand in for `checked`.
  cat > "$tmp/workflows/planted.yml" <<'YAML'
env:
  RUST_BACKTRACE: 1
jobs:
  publish:
    steps:
      - run: cargo publish --dry-run
YAML
  expect_reject "a workflow set containing only an exempt (no-tests) file"

  # 10. Profile values. `debug = true` is the important one: it is exactly what [profile.test]
  #     already inherits from [profile.dev], so accepting it would be accepting no mitigation.
  printf '[profile.dev]\ndebug = true\n' > "$tmp/Cargo.toml"
  expect_profile_reject "a manifest with no [profile.test] debug"

  printf '[profile.dev]\ndebug = true\n\n[profile.test]\ndebug = true\n' > "$tmp/Cargo.toml"
  expect_profile_reject "[profile.test] debug = true, the full-DWARF state already inherited"

  printf '[profile.dev]\ndebug = true\n\n[profile.test]\ndebug = "line-tables-only"\n' > "$tmp/Cargo.toml"
  expect_profile_accept "a correctly-mitigated manifest"

  echo "self-test passed: the checker rejects a missing mitigation, a job-scoped RUST_BACKTRACE"
  echo "override, RUST_LIB_BACKTRACE: 1 alone, an unmitigated coverage job, a workflow set that"
  echo "verifies nothing, an exempt-only set, a missing [profile.test] debug and debug = true —"
  echo "and accepts a mitigated workflow, trailing-comment and single-quoted mitigations, and"
  echo "debug = \"line-tables-only\"."
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
  exit $?
fi

echo "Backtrace-cost gate (issue #1021):"
rc=0
check_workflows "$repo_root/.github/workflows" || rc=1
check_profile "$repo_root/Cargo.toml" || rc=1

if [ "$rc" -ne 0 ]; then
  echo "" >&2
  echo "FAILED: see scripts/verify-backtrace-env.sh for why this matters." >&2
  exit 1
fi
echo "OK"
