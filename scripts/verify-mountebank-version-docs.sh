#!/usr/bin/env bash
#
# The docs say which Mountebank Rift is compatible with ("compatible with **Mountebank X.Y.Z**", the
# latest release). That claim is only true while it names the version the differential oracle runs,
# `MB_VERSION` in .github/workflows/ci.yml. This fails when any statement names another version, or
# when a page that should carry the statement lost it, so bumping the oracle cannot leave the docs
# advertising the old release.
#
# Usage:
#   scripts/verify-mountebank-version-docs.sh              # check the repo (exit 1 on a mismatch)
#   scripts/verify-mountebank-version-docs.sh --self-test  # prove it flags a planted mismatch
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Overridable for the self-test.
ROOT="${ROOT:-$repo_root}"

# Pages that must state the compatible version.
PAGES=(README.md docs/index.md docs/mountebank/index.md docs/getting-started/migration.md)

pinned_version() {
  local found
  found="$(grep -oE 'MB_VERSION: "[0-9]+\.[0-9]+\.[0-9]+"' "$ROOT/.github/workflows/ci.yml" |
    grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -u)"
  if [[ -z "$found" || "$(wc -l <<<"$found")" -ne 1 ]]; then
    echo "ERROR: expected exactly one MB_VERSION in ci.yml, found: ${found:-none}" >&2
    return 2
  fi
  echo "$found"
}

run_check() {
  local version bad=() page
  version="$(pinned_version)" || return 2

  for page in "${PAGES[@]}"; do
    if ! grep -qF "compatible with **Mountebank $version**" "$ROOT/$page"; then
      bad+=("$page: no \"compatible with **Mountebank $version**\" statement")
    fi
    while IFS= read -r stale; do
      [[ -n "$stale" ]] && bad+=("$page: $stale")
    done < <(grep -oE 'compatible with \*\*Mountebank [0-9]+\.[0-9]+\.[0-9]+\*\*' "$ROOT/$page" |
      grep -vF "Mountebank $version**" || true)
  done

  # differences.md states the reference version in its opening sentence.
  if ! grep -qF "the way Mountebank $version does" "$ROOT/docs/mountebank/differences.md"; then
    bad+=("docs/mountebank/differences.md: opening sentence does not name Mountebank $version")
  fi

  if (( ${#bad[@]} > 0 )); then
    echo "Mountebank version in docs does not match MB_VERSION ($version):" >&2
    printf '  - %s\n' "${bad[@]}" >&2
    return 1
  fi
  echo "Mountebank version docs OK: every page names Mountebank $version (MB_VERSION)."
}

self_test() {
  local tmp out rc=0
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/.github/workflows" "$tmp/docs/mountebank" "$tmp/docs/getting-started"
  echo '  MB_VERSION: "3.0.0"' >"$tmp/.github/workflows/ci.yml"
  for page in "${PAGES[@]}"; do echo 'Rift is compatible with **Mountebank 3.0.0**.' >"$tmp/$page"; done
  echo 'Rift answers the way Mountebank 3.0.0 does.' >"$tmp/docs/mountebank/differences.md"

  out="$(ROOT="$tmp" run_check 2>&1)" || rc=$?
  if (( rc != 0 )); then
    echo "SELF-TEST FAILED: a consistent tree was rejected:" >&2; echo "$out" >&2; return 1
  fi

  echo 'Also compatible with **Mountebank 2.9.5**.' >>"$tmp/docs/index.md"
  rc=0; out="$(ROOT="$tmp" run_check 2>&1)" || rc=$?
  if (( rc != 1 )) || ! grep -qF "docs/index.md" <<<"$out"; then
    echo "SELF-TEST FAILED: a stale version was not flagged (rc=$rc):" >&2; echo "$out" >&2; return 1
  fi

  echo 'no statement here' >"$tmp/docs/index.md"
  rc=0; out="$(ROOT="$tmp" run_check 2>&1)" || rc=$?
  if (( rc != 1 )) || ! grep -qF "no \"compatible with" <<<"$out"; then
    echo "SELF-TEST FAILED: a missing statement was not flagged (rc=$rc):" >&2; echo "$out" >&2; return 1
  fi

  echo "Self-test OK: flags a stale version and a missing statement, accepts a consistent tree."
}

case "${1:-}" in
  --self-test) self_test ;;
  "")          run_check ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 64 ;;
esac
