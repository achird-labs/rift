#!/usr/bin/env bash
#
# Docs install-snippet version gate.
#
# The install commands in README.md and docs/ embed a release version, because a release asset URL
# and `rift-fetch -version` both name one — there is no "latest" spelling that works. So every one
# of them silently rots the moment a release lands, and nothing noticed: the docs shipped a
# `rift-http-proxy-linux-x86_64` asset name that had never existed, and the site footer sat four
# releases behind (v0.13.0 while the engine was v0.17.0) for three weeks.
#
# `release.yml` bumps `docs/_data/rift.yml` and runs this with `--fix`, so the snippets follow
# releases on their own. This gate exists for the other direction: a hand-edited snippet, or a new
# one added at a version that has since moved on, fails CI instead of shipping a command that 404s.
#
# DISCOVERY, NOT A FILE LIST. Snippets are found by their two unambiguous shapes (below), never
# enumerated — a list would go stale exactly when someone adds the snippet nobody remembered to
# register. The shapes are narrow enough that prose cannot match:
#
#   VERSION=vX.Y.Z                                  shell variable in a download snippet
#   rift-fetch@latest -version vX.Y.Z               the rift-go native-library fetch
#
# DELIBERATELY NOT COVERED — these are versions that must NOT track the newest release:
#
#   docs/embedding/ffi.md   "As of v0.11.3 (#429)"  historical statements about when behaviour
#                           "(v0.11.2, #425/#426)"  changed; rewriting them would falsify history
#   docs/sdk/index.md       SDK versions + engine   each SDK's own floor, maintained by that SDK's
#                           floor columns           bump automation; a floor is what it is TESTED
#                                                   against, not the newest engine
#   docs/_config.yml        "Docs current as of"    a human-reconciliation claim (see sync.yml)
#   docs/_data/sync.yml     release:/commit:/date:  ditto — automation must never assert a review
#
# The gate is HERMETIC: it compares the snippets against the declared value in docs/_data/rift.yml
# and never calls the GitHub API, so it cannot flake on a network blip or fork-PR token. Keeping
# that value honest is the release workflow's job, not this script's.
#
# Usage:
#   scripts/verify-docs-version.sh              # check (exit 1 on any mismatch)
#   scripts/verify-docs-version.sh --fix        # rewrite every snippet to the declared version
#   scripts/verify-docs-version.sh --set vX.Y.Z # set the declared version, then --fix
#   scripts/verify-docs-version.sh --self-test  # prove the checker flags a planted drift
#
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Overridable so the self-test can point the checks at a scratch copy of the tree.
ROOT="${ROOT:-$repo_root}"
VERSION_FILE="${VERSION_FILE:-$ROOT/docs/_data/rift.yml}"

log()  { echo "[docs-version] $*"; }
fail() { echo "[FAIL] $*" >&2; }

# --- the declared version ---------------------------------------------------

read_declared() {
  local v
  # Deliberately not `yq`: this must run on a bare runner. The key is a plain scalar.
  v="$(sed -n 's/^latest_release:[[:space:]]*\(v[0-9][0-9.]*\).*/\1/p' "$VERSION_FILE" | head -1)"
  if [ -z "$v" ]; then
    fail "no 'latest_release: vX.Y.Z' in $VERSION_FILE"
    return 1
  fi
  printf '%s' "$v"
}

write_declared() {
  local new="$1" tmp
  tmp="$(mktemp)"
  sed "s/^latest_release:[[:space:]]*v[0-9][0-9.]*.*/latest_release: ${new}/" "$VERSION_FILE" >"$tmp"
  mv "$tmp" "$VERSION_FILE"
}

# --- snippet discovery ------------------------------------------------------

# Files that may carry install snippets. README.md is included because it is the first thing a
# visitor copy-pastes and is NOT part of the Jekyll site, so it cannot interpolate a data value —
# the literal has to be kept in step by this script.
snippet_files() {
  { echo "$ROOT/README.md"
    find "$ROOT/docs" -name '*.md' -type f
  } | sort -u
}

# Emit "file:line:found-version" for every install snippet whose version != the declared one.
# The two patterns are anchored tightly so prose like "since v0.11.2" can never match.
drifted() {
  local want="$1" f
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    grep -nE "(^|[[:space:]])VERSION=v[0-9]+\.[0-9]+\.[0-9]+|rift-fetch@latest -version v[0-9]+\.[0-9]+\.[0-9]+" "$f" 2>/dev/null \
      | grep -vF "$want" \
      | while IFS= read -r hit; do
          printf '%s:%s\n' "${f#"$ROOT"/}" "$hit"
        done
  done < <(snippet_files)
}

fix_files() {
  local want="$1" f tmp n=0
  while IFS= read -r f; do
    [ -f "$f" ] || continue
    tmp="$(mktemp)"
    sed -E "s/(^|[[:space:]])VERSION=v[0-9]+\.[0-9]+\.[0-9]+/\1VERSION=${want}/g; \
            s/(rift-fetch@latest -version )v[0-9]+\.[0-9]+\.[0-9]+/\1${want}/g" "$f" >"$tmp"
    if ! cmp -s "$f" "$tmp"; then
      mv "$tmp" "$f"
      log "updated ${f#"$ROOT"/}"
      n=$((n + 1))
    else
      rm -f "$tmp"
    fi
  done < <(snippet_files)
  log "$n file(s) rewritten to $want"
}

check() {
  local want out
  want="$(read_declared)"
  out="$(drifted "$want" || true)"
  if [ -n "$out" ]; then
    fail "install snippet(s) disagree with docs/_data/rift.yml (latest_release: $want):"
    echo "$out" | sed 's/^/  /' >&2
    echo >&2
    echo "Fix with: scripts/verify-docs-version.sh --fix" >&2
    return 1
  fi
  log "OK: every install snippet names $want"
}

# --- self-test --------------------------------------------------------------
#
# Proves the checker actually flags drift rather than passing vacuously — the same failure mode
# that let the stale docs ship. Runs against a throwaway copy so the working tree is untouched.
self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  mkdir -p "$tmp/docs/_data"
  cp "$VERSION_FILE" "$tmp/docs/_data/rift.yml"
  printf 'VERSION=v9.9.9\n' >"$tmp/README.md"

  # 1. planted drift must FAIL
  if ROOT="$tmp" VERSION_FILE="$tmp/docs/_data/rift.yml" "$SELF" >/dev/null 2>&1; then
    echo "verify-docs-version --self-test: FAILED — checker passed a planted drift." >&2
    return 1
  fi

  # 2. --fix must repair it, and the repaired tree must PASS
  ROOT="$tmp" VERSION_FILE="$tmp/docs/_data/rift.yml" "$SELF" --fix >/dev/null 2>&1
  if ! ROOT="$tmp" VERSION_FILE="$tmp/docs/_data/rift.yml" "$SELF" >/dev/null 2>&1; then
    echo "verify-docs-version --self-test: FAILED — checker rejected a correctly-synced tree." >&2
    return 1
  fi

  # 3. a historical reference must NOT be rewritten (the ffi.md failure mode)
  printf 'As of v0.11.3 (#429), passing both caCertPath and caKeyPath\n' >"$tmp/docs/history.md"
  ROOT="$tmp" VERSION_FILE="$tmp/docs/_data/rift.yml" "$SELF" --fix >/dev/null 2>&1
  if ! grep -q 'v0.11.3' "$tmp/docs/history.md"; then
    echo "verify-docs-version --self-test: FAILED — rewrote a historical version reference." >&2
    return 1
  fi

  echo "verify-docs-version --self-test: OK — flags drift, repairs it, leaves history alone."
}

# --- main -------------------------------------------------------------------

case "${1:---check}" in
  --check) check ;;
  --fix)   fix_files "$(read_declared)"; check ;;
  --set)
    [ $# -ge 2 ] || { echo "usage: $0 --set vX.Y.Z" >&2; exit 64; }
    case "$2" in
      v[0-9]*.[0-9]*.[0-9]*) ;;
      *) echo "$0: --set expects a vX.Y.Z tag, got '$2'" >&2; exit 64 ;;
    esac
    write_declared "$2"
    log "declared version set to $2"
    fix_files "$2"
    check
    ;;
  --self-test) self_test ;;
  -h | --help) echo "usage: $0 [--check | --fix | --set vX.Y.Z | --self-test]" >&2; exit 64 ;;
  *) echo "$0: unknown option '$1'" >&2; exit 64 ;;
esac
