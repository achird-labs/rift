#!/usr/bin/env bash
#
# Package sdk-conformance-<version>.tar.gz (issue #460): the vendored SDK-conformance corpus that
# every official Rift SDK's CI replays over embedded and remote transports. The tarball unpacks to a
# single versioned root `sdk-conformance-<version>/` containing README.md, manifest.json (with the
# real engineVersion stamped in — the checked-in copy carries a `0.0.0-dev` placeholder), and the
# corpus/ tree. A `.sha256` sidecar is emitted alongside so consumers can verify the download.
#
# The corpus itself is engine-canonical and is proven to serve + verify on this commit by
# `crates/rift-http-proxy/tests/corpus_replay.rs`; this script only packages it.
#
# Usage:
#   scripts/gen-sdk-conformance.sh <version> [output.tar.gz]   # package the corpus
#   scripts/gen-sdk-conformance.sh --self-test                 # prove the packager works
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/sdk-conformance"

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

# Package $SRC_DIR into <out> as `sdk-conformance-<version>/…`, stamping engineVersion=<version>.
package() {
  local version="$1" out="${2:-sdk-conformance-$1.tar.gz}"
  [ -n "$version" ] || fail "version argument is required"
  [ -d "$SRC_DIR" ] || fail "corpus source not found: $SRC_DIR"
  [ -f "$SRC_DIR/manifest.json" ] || fail "manifest.json missing under $SRC_DIR"
  [ -d "$SRC_DIR/corpus/imposters" ] || fail "corpus/imposters missing under $SRC_DIR"
  command -v jq >/dev/null 2>&1 || fail "jq is required"

  # Absolutize the output path before we cd around.
  mkdir -p "$(dirname "$out")"
  out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"

  local stage root
  stage="$(mktemp -d)"; STAGE_DIR="$stage"
  root="$stage/sdk-conformance-$version"
  mkdir -p "$root"

  cp "$SRC_DIR/README.md" "$root/"
  cp -R "$SRC_DIR/corpus" "$root/"
  # Stamp the real engine version into the packaged manifest (checked-in copy is a placeholder).
  jq --arg v "$version" '.engineVersion = $v' "$SRC_DIR/manifest.json" > "$root/manifest.json"

  tar -czf "$out" -C "$stage" "sdk-conformance-$version"
  ( cd "$(dirname "$out")" && sha256_of "$(basename "$out")" > "$(basename "$out").sha256" )

  echo "[ok] wrote $out"
  echo "[ok] wrote $out.sha256"
}

# Prove the packager end-to-end: version is stamped, structure is intact, and every corpus fixture
# survives the round-trip. No engine build required.
self_test() {
  local work ver="9.9.9-selftest"
  work="$(mktemp -d)"; WORK_DIR="$work"

  package "$ver" "$work/out.tar.gz"
  [ -f "$work/out.tar.gz" ] || fail "tarball not produced"
  [ -f "$work/out.tar.gz.sha256" ] || fail "checksum not produced"

  tar -xzf "$work/out.tar.gz" -C "$work"
  local extracted="$work/sdk-conformance-$ver"
  [ -f "$extracted/README.md" ] || fail "README.md missing from tarball"
  [ -f "$extracted/manifest.json" ] || fail "manifest.json missing from tarball"
  [ -d "$extracted/corpus/imposters" ] || fail "corpus/imposters missing from tarball"

  local stamped
  stamped="$(jq -r '.engineVersion' "$extracted/manifest.json")"
  [ "$stamped" = "$ver" ] || fail "engineVersion not stamped (got '$stamped', want '$ver')"

  local disk pkg
  disk="$(find "$SRC_DIR/corpus/imposters" -name '*.json' | wc -l | tr -d ' ')"
  pkg="$(find "$extracted/corpus/imposters" -name '*.json' | wc -l | tr -d ' ')"
  [ "$disk" = "$pkg" ] || fail "fixture count drift (disk=$disk, packaged=$pkg)"
  local listed
  listed="$(jq '.fixtures | length' "$extracted/manifest.json")"
  [ "$listed" = "$pkg" ] || fail "manifest lists $listed fixtures but $pkg are packaged"

  echo "[ok] self-test passed ($pkg fixtures, version stamped, checksum written)"
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
