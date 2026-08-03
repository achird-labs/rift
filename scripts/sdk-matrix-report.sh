#!/usr/bin/env bash
#
# SDK drift reporter (issue #462).
#
# `sdk-matrix.yml` runs each released SDK's conformance lane against the *latest* engine release.
# When a lane goes red, that is engine↔SDK drift and someone has to look at it — but a scheduled
# workflow has no PR to turn red, so the signal has to become an issue. Filing one per run would
# bury the repo under a daily duplicate, so this keeps exactly one open tracking issue per SDK and
# appends to it instead.
#
# Dedupe is by an HTML-comment marker in the issue body, matched against `gh issue list` (a direct
# API listing) rather than `gh issue search`. The search index is eventually consistent: an issue
# filed by this morning's run is not reliably searchable by this afternoon's, and every miss is a
# duplicate — the one failure mode this script exists to prevent.
#
# Usage:
#   scripts/sdk-matrix-report.sh --sdk java --tag v0.2.2 --engine v0.16.0 [--run-url URL]
#   scripts/sdk-matrix-report.sh --self-test    # prove create-then-update never duplicates
#
set -euo pipefail

# Indirection so --self-test can substitute a stub for the real CLI.
GH="${GH:-gh}"
REPO="${SDK_MATRIX_REPO:-achird-labs/rift}"

marker_for() { printf '<!-- sdk-matrix-drift:%s -->' "$1"; }

usage() {
  sed -n '3,18p' "${BASH_SOURCE[0]}" >&2
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

# --- self-test ---------------------------------------------------------------------------------
# Runs the real code path twice against a stub `gh` and asserts exactly one issue was created and
# the second run commented instead. A reporter that silently duplicates would still "work" in the
# workflow, so this is the only place the dedupe is actually proven.
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
    printf 'comment\n' >>"$state/calls"
    ;;
  *)
    printf 'unexpected stub call: %s\n' "$*" >&2; exit 1 ;;
esac
STUB
  chmod +x "$tmp/gh"

  export SDK_MATRIX_STUB_STATE="$tmp"
  printf '[]\n' >"$tmp/issues.json"
  : >"$tmp/calls"

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

  if [ "$status" = "0" ]; then
    printf 'self-test: OK — create once, then update; per-SDK markers do not collide\n'
  fi
  return "$status"
}

sdk="" tag="" engine="" run_url=""
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test) self_test; exit $? ;;
    --sdk)     sdk="${2:-}"; shift 2 ;;
    --tag)     tag="${2:-}"; shift 2 ;;
    --engine)  engine="${2:-}"; shift 2 ;;
    --run-url) run_url="${2:-}"; shift 2 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage ;;
  esac
done

[ -n "$sdk" ] && [ -n "$tag" ] && [ -n "$engine" ] || usage
report "$sdk" "$tag" "$engine" "$run_url"
