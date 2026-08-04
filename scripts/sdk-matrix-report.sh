#!/usr/bin/env bash
#
# SDK drift reporter (issue #462) and its close-on-green half (issue #915).
#
# `sdk-matrix.yml` runs each released SDK's conformance lane against the *latest* engine release.
# When a lane goes red, that is engine↔SDK drift and someone has to look at it — but a scheduled
# workflow has no PR to turn red, so the signal has to become an issue. Filing one per run would
# bury the repo under a daily duplicate, so this keeps exactly one open tracking issue per SDK and
# appends to it instead. When the lane passes again, `--resolved` closes that issue — without it
# every transient red costs a human close, and a stale open issue is indistinguishable from live
# drift (#915 sat open through two green runs).
#
# Dedupe is by an HTML-comment marker in the issue body, matched against `gh issue list` (a direct
# API listing) rather than `gh issue search`. The search index is eventually consistent: an issue
# filed by this morning's run is not reliably searchable by this afternoon's, and every miss is a
# duplicate — the one failure mode this script exists to prevent.
#
# Usage:
#   scripts/sdk-matrix-report.sh --sdk java --tag v0.2.2 --engine v0.16.0 [--run-url URL]
#   scripts/sdk-matrix-report.sh --resolved --sdk java --tag v0.2.2 --engine v0.16.0 [--run-url URL]
#   scripts/sdk-matrix-report.sh --self-test    # prove create/update/close never misbehaves
#
set -euo pipefail

# Indirection so --self-test can substitute a stub for the real CLI.
GH="${GH:-gh}"
REPO="${SDK_MATRIX_REPO:-achird-labs/rift}"

marker_for() { printf '<!-- sdk-matrix-drift:%s -->' "$1"; }

usage() {
  sed -n '3,21p' "${BASH_SOURCE[0]}" >&2
  exit 2
}

# Emits the number of the open tracking issue for this SDK, or nothing if there is none.
# Returns non-zero if the listing could not be read at all — the caller must not treat that as
# "no issue exists", since doing so files a fresh duplicate on every failed lookup. Checked
# explicitly rather than left to `set -e`, which is suppressed whenever a caller wraps this in a
# conditional.
find_open_tracking_issue() {
  local marker="$1" listing
  if ! listing="$($GH issue list --repo "$REPO" --state open --label sdk --limit 100 --json number,body)"; then
    printf 'error: could not list open issues in %s — refusing to file a possible duplicate\n' "$REPO" >&2
    return 1
  fi
  printf '%s' "$listing" \
    | jq -r --arg m "$marker" 'map(select(.body != null and (.body | contains($m)))) | .[0].number // empty'
}

report() {
  local sdk="$1" tag="$2" engine="$3" run_url="$4"
  local marker existing title body
  marker="$(marker_for "$sdk")"
  title="sdk-matrix: rift-${sdk} ${tag} is red against engine ${engine}"

  if ! existing="$(find_open_tracking_issue "$marker")"; then
    return 1
  fi

  if [ -n "$existing" ]; then
    # Update, never duplicate. A comment keeps the history of which engine versions were red,
    # which is what tells a triager whether this is a new break or a known one still unfixed.
    $GH issue comment "$existing" --repo "$REPO" --body \
      "Still red: rift-${sdk} \`${tag}\` against engine \`${engine}\`.${run_url:+ Run: ${run_url}}"
    printf 'updated #%s\n' "$existing"
    return 0
  fi

  # Written to a file rather than captured with `body="$(cat <<EOF …)"`: bash 3.2 — still what
  # macOS ships, and what a maintainer runs --self-test under — mis-parses an apostrophe inside a
  # heredoc nested in command substitution, and this body has several.
  body="$(mktemp)"
  cat >"$body" <<EOF
${marker}

The scheduled cross-SDK conformance matrix (\`.github/workflows/sdk-matrix.yml\`) found drift.

| | |
|---|---|
| SDK | \`rift-${sdk}\` |
| SDK release | \`${tag}\` |
| Engine release | \`${engine}\` |
${run_url:+| Run | ${run_url} |}

The SDK's own conformance lane passes against the engine version it pins; this gate replays it
against the newest engine release instead. Something in that path failed — the engine may have
broken compatibility, the SDK may need a bump, or the release assets this lane downloads may be
missing or renamed. The run log says which step went red.

This issue is reused for every subsequent red run of this SDK — see the comments for the history.
EOF

  $GH issue create --repo "$REPO" --title "$title" --label sdk --label needs-triage --body-file "$body"
  rm -f "$body"
  printf 'created\n'
}

# The green half of the lifecycle (#915): a passing lane closes its SDK's open tracking issue.
# The error handling is deliberately the mirror image of report()'s. The red path fails CLOSED
# (an unreadable issue list aborts rather than filing a possible duplicate); this path fails
# OPEN: any error here is warned about and swallowed, because issue bookkeeping must never turn
# a green conformance lane red. Worst case the issue stays open until the next green run.
resolve() {
  local sdk="$1" tag="$2" engine="$3" run_url="$4"
  local marker existing
  marker="$(marker_for "$sdk")"

  if ! existing="$(find_open_tracking_issue "$marker")"; then
    printf 'warning: could not list open issues — leaving any rift-%s tracking issue open\n' "$sdk" >&2
    return 0
  fi

  if [ -z "$existing" ]; then
    printf 'no open tracking issue for rift-%s\n' "$sdk"
    return 0
  fi

  # Comment before closing so the "why it closed" lands even if the close then fails — and each
  # step gets its own warning, so a log reader debugs the right gh subcommand.
  if ! $GH issue comment "$existing" --repo "$REPO" --body \
    "Green again: rift-${sdk} \`${tag}\` passed against engine \`${engine}\`.${run_url:+ Run: ${run_url}}"; then
    printf 'warning: could not comment on #%s — leaving it open\n' "$existing" >&2
    return 0
  fi
  if ! $GH issue close "$existing" --repo "$REPO" --reason completed; then
    printf 'warning: commented on #%s but could not close it — leaving it open\n' "$existing" >&2
    return 0
  fi
  printf 'closed #%s\n' "$existing"
}

# --- self-test ---------------------------------------------------------------------------------
# Runs the real code paths against a stub `gh`. The red half proves create-once-then-update (a
# reporter that silently duplicates would still "work" in the workflow); the green half proves
# close-on-green, marker isolation, and the polarity split — red fails closed, green fails open —
# including through the real CLI dispatch the workflow steps invoke. This is the only place any
# of that is actually proven.
self_test() {
  local tmp status=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat >"$tmp/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh: `issue list` replays whatever state/ holds; create/comment record the call.
set -euo pipefail
state="$SDK_MATRIX_STUB_STATE"
case "$1 $2" in
  "issue list")
    cat "$state/issues.json"
    ;;
  "issue create")
    body_file=""
    while [ $# -gt 0 ]; do
      case "$1" in --body-file) body_file="$2"; shift 2 ;; *) shift ;; esac
    done
    body="$(cat "$body_file")"
    printf '%s\n' "$body" >"$state/created-body.txt"
    printf 'create\n' >>"$state/calls"
    # A created issue is immediately visible to the next `issue list`.
    jq -n --arg b "$body" '[{number: 4242, body: $b}]' >"$state/issues.json"
    ;;
  "issue comment")
    # Record the body too: the assertions must be able to tell "Still red" from "Green again".
    body=""
    while [ $# -gt 0 ]; do
      case "$1" in --body) body="$2"; shift 2 ;; *) shift ;; esac
    done
    printf '%s\n' "$body" >>"$state/comment-bodies.txt"
    printf 'comment\n' >>"$state/calls"
    ;;
  "issue close")
    printf 'close %s\n' "$3" >>"$state/calls"
    # A closed issue disappears from the next open-issues listing.
    printf '[]\n' >"$state/issues.json"
    ;;
  *)
    printf 'unexpected stub call: %s\n' "$*" >&2; exit 1 ;;
esac
STUB
  chmod +x "$tmp/gh"

  export SDK_MATRIX_STUB_STATE="$tmp"
  printf '[]\n' >"$tmp/issues.json"
  : >"$tmp/calls"
  : >"$tmp/comment-bodies.txt"

  GH="$tmp/gh" report go v0.1.0 v0.16.0 "https://example.invalid/run/1" >/dev/null
  GH="$tmp/gh" report go v0.1.0 v0.17.0 "https://example.invalid/run/2" >/dev/null

  local creates comments
  creates="$(grep -c '^create$' "$tmp/calls" || true)"
  comments="$(grep -c '^comment$' "$tmp/calls" || true)"

  if [ "$creates" != "1" ]; then
    printf 'FAIL: expected exactly 1 issue creation, got %s\n' "$creates" >&2
    status=1
  fi
  if [ "$comments" != "1" ]; then
    printf 'FAIL: expected the second red run to comment once, got %s\n' "$comments" >&2
    status=1
  fi
  case "$(cat "$tmp/comment-bodies.txt")" in
    *"Still red"*) ;;
    *) printf 'FAIL: the second red run comment is missing "Still red"\n' >&2; status=1 ;;
  esac

  # The issue has to name the SDK, its tag and the engine version — a tracking issue that does not
  # is unactionable, and the marker is what makes the next run find it.
  local created; created="$(cat "$tmp/created-body.txt")"
  local needle
  for needle in '<!-- sdk-matrix-drift:go -->' 'rift-go' 'v0.1.0' 'v0.16.0'; do
    case "$created" in
      *"$needle"*) ;;
      *) printf 'FAIL: created issue body is missing %s\n' "$needle" >&2; status=1 ;;
    esac
  done

  # An unrelated SDK's open issue must not be mistaken for this one's.
  jq -n '[{number: 7, body: "<!-- sdk-matrix-drift:java -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  GH="$tmp/gh" report go v0.1.0 v0.16.0 "" >/dev/null
  if [ "$(grep -c '^create$' "$tmp/calls" || true)" != "1" ]; then
    printf 'FAIL: a different SDK'\''s tracking issue was matched as this one'\''s\n' >&2
    status=1
  fi

  # Fail closed when the lookup itself fails. An unreadable issue list is indistinguishable from
  # "no tracking issue exists", and guessing the wrong one of those files a duplicate every run —
  # the exact failure this script exists to prevent. So it must abort, not create.
  printf '#!/usr/bin/env bash\nexit 1\n' >"$tmp/gh-broken"
  chmod +x "$tmp/gh-broken"
  : >"$tmp/calls"
  if GH="$tmp/gh-broken" report go v0.1.0 v0.16.0 "" >/dev/null 2>&1; then
    printf 'FAIL: a failing issue lookup was treated as "no open issue"\n' >&2
    status=1
  fi

  # --- close-on-green (--resolved) — the other half of the lifecycle (#915) --------------------

  # red → green: the open tracking issue gets exactly one "Green again" comment and one close.
  jq -n '[{number: 4242, body: "<!-- sdk-matrix-drift:go -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  : >"$tmp/comment-bodies.txt"
  GH="$tmp/gh" resolve go v0.1.0 v0.17.0 "https://example.invalid/run/3" >/dev/null
  if [ "$(grep -c '^close 4242$' "$tmp/calls" || true)" != "1" ] \
    || [ "$(grep -c '^comment$' "$tmp/calls" || true)" != "1" ]; then
    printf 'FAIL: a green run over an open tracking issue must comment once and close once\n' >&2
    status=1
  fi
  case "$(cat "$tmp/comment-bodies.txt")" in
    *"Green again"*) ;;
    *) printf 'FAIL: the closing comment is missing "Green again"\n' >&2; status=1 ;;
  esac

  # Green with no open tracking issue — the everyday case — must not comment on or close anything.
  printf '[]\n' >"$tmp/issues.json"
  : >"$tmp/calls"
  GH="$tmp/gh" resolve go v0.1.0 v0.17.0 "" >/dev/null
  if [ -s "$tmp/calls" ]; then
    printf 'FAIL: a green run with no tracking issue must not comment or close anything\n' >&2
    status=1
  fi

  # Marker isolation on the green path, mirroring the red-path case above.
  jq -n '[{number: 7, body: "<!-- sdk-matrix-drift:java -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  GH="$tmp/gh" resolve go v0.1.0 v0.17.0 "" >/dev/null
  if [ -s "$tmp/calls" ]; then
    printf 'FAIL: a different SDK'\''s tracking issue was closed by this SDK'\''s green run\n' >&2
    status=1
  fi

  # Fail OPEN when the lookup fails in --resolved mode — the mirror image of the red path's
  # fail-closed case: issue bookkeeping must never turn a green conformance lane red.
  if ! GH="$tmp/gh-broken" resolve go v0.1.0 v0.17.0 "" >/dev/null 2>&1; then
    printf 'FAIL: a failing issue lookup in --resolved mode must not fail the green lane\n' >&2
    status=1
  fi

  # Comment-lands-then-close-fails split — the ordering guarantee resolve() documents. This stub
  # forwards everything except `issue close` to the normal stub, so the comment is recorded.
  cat >"$tmp/gh-close-fails" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [ "$1 $2" = "issue close" ]; then exit 1; fi
exec "$(dirname "$0")/gh" "$@"
STUB
  chmod +x "$tmp/gh-close-fails"
  jq -n '[{number: 4242, body: "<!-- sdk-matrix-drift:go -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  : >"$tmp/comment-bodies.txt"
  local green_out
  if ! green_out="$(GH="$tmp/gh-close-fails" resolve go v0.1.0 v0.17.0 "" 2>&1)"; then
    printf 'FAIL: a failing close must not fail the green lane\n' >&2
    status=1
  fi
  if [ "$(grep -c '^comment$' "$tmp/calls" || true)" != "1" ]; then
    printf 'FAIL: the "Green again" comment must land before the close is attempted\n' >&2
    status=1
  fi
  case "$green_out" in
    *"closed #"*) printf 'FAIL: resolve claimed "closed" though the close failed\n' >&2; status=1 ;;
  esac
  case "$green_out" in
    *"could not close"*) ;;
    *) printf 'FAIL: a failed close must be warned about, not silent\n' >&2; status=1 ;;
  esac

  # The workflow's exact CLI shape, through the real arg parser — the direct resolve() calls
  # above bypass the dispatch the "Close drift issue" step depends on.
  jq -n '[{number: 4242, body: "<!-- sdk-matrix-drift:go -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  if ! GH="$tmp/gh" bash "${BASH_SOURCE[0]}" --resolved --sdk go --tag v0.1.0 \
    --engine v0.17.0 --run-url "https://example.invalid/run/4" >/dev/null; then
    printf 'FAIL: the --resolved CLI invocation must exit 0\n' >&2
    status=1
  fi
  if [ "$(grep -c '^close 4242$' "$tmp/calls" || true)" != "1" ]; then
    printf 'FAIL: the --resolved CLI invocation did not close the tracking issue\n' >&2
    status=1
  fi

  # An empty --tag is runtime data (a blank tagName from the SDK release lookup), not an
  # authoring error: the green path must swallow it and still close, never exit 2 — while the
  # red path must keep rejecting it loudly. This is the validation-polarity split at the
  # dispatch layer, the one place resolve()'s own fail-open cannot reach.
  jq -n '[{number: 4242, body: "<!-- sdk-matrix-drift:go -->"}]' >"$tmp/issues.json"
  : >"$tmp/calls"
  if ! GH="$tmp/gh" bash "${BASH_SOURCE[0]}" --resolved --sdk go --tag "" \
    --engine v0.17.0 >/dev/null 2>&1; then
    printf 'FAIL: an empty --tag must not redden the green lane\n' >&2
    status=1
  fi
  if [ "$(grep -c '^close 4242$' "$tmp/calls" || true)" != "1" ]; then
    printf 'FAIL: an empty --tag must still let the green run close the tracking issue\n' >&2
    status=1
  fi
  if GH="$tmp/gh" bash "${BASH_SOURCE[0]}" --sdk go --tag "" --engine v0.17.0 >/dev/null 2>&1; then
    printf 'FAIL: the red path must still reject a missing --tag loudly\n' >&2
    status=1
  fi

  if [ "$status" = "0" ]; then
    printf 'self-test: OK — create once, then update, close on green; per-SDK markers do not collide\n'
  fi
  return "$status"
}

mode=report sdk="" tag="" engine="" run_url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) self_test; exit $? ;;
    --resolved) mode=resolve; shift ;;
    --sdk)     sdk="${2:-}"; shift 2 ;;
    --tag)     tag="${2:-}"; shift 2 ;;
    --engine)  engine="${2:-}"; shift 2 ;;
    --run-url) run_url="${2:-}"; shift 2 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage ;;
  esac
done

# Validation polarity follows the mode's failure polarity. The red path insists on all three
# values — a malformed report is worth failing loudly over. The green path's fail-open contract
# has to hold HERE too, before resolve() is ever reached: --tag arrives empty whenever the SDK's
# release lookup yields a blank tagName, and an exit 2 from this line would redden a green lane.
# Only --sdk is load-bearing for a close; an empty tag/engine merely blunts the comment text.
# (A genuinely unknown flag still exits 2 above in either mode: that is a workflow-authoring
# bug, best caught loudly on its first run — the self-test drives this exact CLI shape.)
if [ "$mode" = report ]; then
  [ -n "$sdk" ] && [ -n "$tag" ] && [ -n "$engine" ] || usage
elif [ -z "$sdk" ]; then
  printf 'warning: --resolved without --sdk — nothing to close\n' >&2
  exit 0
fi

# Belt and braces on the same contract: even if resolve() ever leaks a non-zero status through
# a path the self-test missed, the green lane stays green.
if [ "$mode" = resolve ]; then
  resolve "$sdk" "$tag" "$engine" "$run_url" \
    || printf 'warning: close-on-green bookkeeping failed — leaving any tracking issue open\n' >&2
  exit 0
fi
report "$sdk" "$tag" "$engine" "$run_url"
