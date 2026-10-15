#!/usr/bin/env bash
#
# Package rift-conformance-<version>.tar.gz (issue #1339): the Plane A specification — the Gherkin
# features and the fixtures they load — that the rift-conformance runner executes against a rift
# build when `RIFT_FEATURES_DIR` points at it. The tarball unpacks to a single versioned root
# `rift-conformance-<version>/` containing README.md, manifest.json (with the real engineVersion
# stamped in — the checked-in copy carries a `0.0.0-dev` placeholder), features/, rift-fixtures/,
# rift-lint-fixtures/ and rift-scripts/. conformance/differential/ (the Mountebank oracle) is not
# part of it. A `.sha256` sidecar is emitted alongside so consumers can verify the download.
#
# The manifest is proven to match the tree by `crates/rift-http-proxy/tests/plane_a_manifest.rs`;
# this script only packages it.
#
# Usage:
#   scripts/gen-conformance-features.sh <version> [output.tar.gz]   # package the tree
#   scripts/gen-conformance-features.sh --self-test                 # prove the packager works
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/conformance"
TREES=(features rift-fixtures rift-lint-fixtures rift-scripts)

# Clean up temp dirs on ANY exit (including an early `set -e`/`fail()` abort) so nothing leaks.
# Assigned directly (not via a command-substitution helper, which would run in a subshell and never
# reach these). `return 0` so the trap never poisons the script's exit status.
STAGE_DIR=""
WORK_DIR=""
cleanup() {
  [ -n "$STAGE_DIR" ] && rm -rf "$STAGE_DIR"
  [ -n "$WORK_DIR" ] && rm -rf "$WORK_DIR"
  return 0
}
trap cleanup EXIT

fail() { echo "[FAIL] $*" >&2; exit 1; }

# Portable sha256 (Linux coreutils vs macOS/BSD).
sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"
  else shasum -a 256 "$1"
  fi
}

# Package $SRC_DIR into <out> as `rift-conformance-<version>/…`, stamping engineVersion=<version>.
package() {
  local version="$1" out="${2:-rift-conformance-$1.tar.gz}"
  [ -n "$version" ] || fail "version argument is required"
  [ -d "$SRC_DIR" ] || fail "conformance tree not found: $SRC_DIR"
  [ -f "$SRC_DIR/manifest.json" ] || fail "manifest.json missing under $SRC_DIR"
  [ -f "$SRC_DIR/README.md" ] || fail "README.md missing under $SRC_DIR"
  local tree
  for tree in "${TREES[@]}"; do
    [ -d "$SRC_DIR/$tree" ] || fail "$tree/ missing under $SRC_DIR"
  done
  command -v jq >/dev/null 2>&1 || fail "jq is required"

  # Absolutize the output path before we cd around.
  mkdir -p "$(dirname "$out")"
  out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"

  local stage root
  stage="$(mktemp -d)"; STAGE_DIR="$stage"
  root="$stage/rift-conformance-$version"
  mkdir -p "$root"

  cp "$SRC_DIR/README.md" "$root/"
  for tree in "${TREES[@]}"; do
    cp -R "$SRC_DIR/$tree" "$root/"
  done
  # Stamp the real engine version into the packaged manifest (checked-in copy is a placeholder).
  jq --arg v "$version" '.engineVersion = $v' "$SRC_DIR/manifest.json" > "$root/manifest.json"

  tar -czf "$out" -C "$stage" "rift-conformance-$version"
  ( cd "$(dirname "$out")" && sha256_of "$(basename "$out")" > "$(basename "$out").sha256" )

  echo "[ok] wrote $out"
  echo "[ok] wrote $out.sha256"
}

# Prove the packager end-to-end: version is stamped, structure is intact, every feature and fixture
# survives the round-trip, and the oracle tree stays out. No engine build required.
self_test() {
  local work ver="9.9.9-selftest"
  work="$(mktemp -d)"; WORK_DIR="$work"

  package "$ver" "$work/out.tar.gz"
  [ -f "$work/out.tar.gz" ] || fail "tarball not produced"
  [ -f "$work/out.tar.gz.sha256" ] || fail "checksum not produced"

  tar -xzf "$work/out.tar.gz" -C "$work"
  local extracted="$work/rift-conformance-$ver"
  [ -f "$extracted/README.md" ] || fail "README.md missing from tarball"
  [ -f "$extracted/manifest.json" ] || fail "manifest.json missing from tarball"
  [ ! -e "$extracted/differential" ] || fail "differential/ must not be packaged"

  local stamped
  stamped="$(jq -r '.engineVersion' "$extracted/manifest.json")"
  [ "$stamped" = "$ver" ] || fail "engineVersion not stamped (got '$stamped', want '$ver')"

  local tree disk pkg
  for tree in "${TREES[@]}"; do
    disk="$(find "$SRC_DIR/$tree" -type f | wc -l | tr -d ' ')"
    pkg="$(find "$extracted/$tree" -type f | wc -l | tr -d ' ')"
    [ "$disk" = "$pkg" ] || fail "$tree/ file count drift (disk=$disk, packaged=$pkg)"
  done
  # A file git ignores (e.g. `*.pem` under the repo-wide secrets rules) is on disk locally but
  # absent from every CI checkout and release, so both sides of the counts above would agree on
  # a tree missing it. Refuse any ignored file under the tree.
  if git -C "$REPO_ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    local ignored
    ignored="$(git -C "$REPO_ROOT" ls-files --others --ignored --exclude-standard -- conformance \
      | grep -v '^conformance/differential/' || true)"
    [ -z "$ignored" ] || fail "git ignores these conformance files, so releases would lack them:
$ignored"
  fi

  local features listed
  features="$(find "$extracted/features/rift" -name '*.feature' | wc -l | tr -d ' ')"
  listed="$(jq '.features | length' "$extracted/manifest.json")"
  [ "$listed" = "$features" ] || fail "manifest lists $listed features but $features are packaged"

  echo "[ok] self-test passed ($features features, fixtures intact, version stamped, checksum written)"
}

main() {
  case "${1:-}" in
    --self-test) self_test ;;
    "" | -h | --help)
      echo "Usage: $0 <version> [output.tar.gz] | $0 --self-test" >&2
      exit 1
      ;;
    *) package "$@" ;;
  esac
}

main "$@"
