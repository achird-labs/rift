#!/usr/bin/env bash
#
# Package rift-templates-<version>.tar.gz (issue #1281): the vendor-mock template catalog under
# templates/. The tarball unpacks to a single versioned root `rift-templates-<version>/` holding the
# catalog README.md and one directory per template, each with its manifest's `requires.rift` stamped
# to `>=<version>` (the engine release the packaged templates were gated against). A `.sha256`
# sidecar is emitted alongside so consumers can verify the download.
#
# A template is a directory (its entrypoint inlines fixtures with EJS `stringify`, which is refused
# for `https:` sources), so this is a tarball to extract and load with `--configfile`, not a URL.
#
# The templates are proven to load, lint clean and pass their smoke tests on this commit by
# `crates/rift-mock-core/tests/shipped_templates_load.rs`, rift-lint's
# `the_shipped_templates_lint_clean` and `scripts/verify-templates.sh`; this script only packages.
#
# Usage:
#   scripts/gen-templates.sh <version> [output.tar.gz]   # package the catalog
#   scripts/gen-templates.sh --self-test                 # prove the packager works
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIR="$REPO_ROOT/templates"

# Clean up temp dirs on ANY exit (including an early `set -e`/`fail()` abort) so nothing leaks.
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

# Template directories under $SRC_DIR, one per line.
template_dirs() {
  local dir
  for dir in "$SRC_DIR"/*/; do
    [ -d "$dir" ] && basename "$dir"
  done
  return 0
}

# Package $SRC_DIR into <out> as `rift-templates-<version>/…`, stamping requires.rift.
package() {
  local version="$1" out="${2:-rift-templates-$1.tar.gz}"
  [ -n "$version" ] || fail "version argument is required"
  [ -d "$SRC_DIR" ] || fail "templates source not found: $SRC_DIR"
  [ -f "$SRC_DIR/README.md" ] || fail "README.md missing under $SRC_DIR"
  command -v jq >/dev/null 2>&1 || fail "jq is required"
  local names
  names="$(template_dirs)"
  [ -n "$names" ] || fail "no template directory under $SRC_DIR"

  # The release tag is `v<semver>`; a version requirement names the bare semver.
  local semver="${version#v}"

  mkdir -p "$(dirname "$out")"
  out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"

  local stage root name
  stage="$(mktemp -d)"; STAGE_DIR="$stage"
  root="$stage/rift-templates-$version"
  mkdir -p "$root"
  cp "$SRC_DIR/README.md" "$root/"
  while IFS= read -r name; do
    [ -f "$SRC_DIR/$name/template.json" ] || fail "$name: template.json missing"
    # Local leftovers from running a template (a venv, an exported CA, caches) are not catalog files.
    tar -C "$SRC_DIR" --exclude=.venv --exclude=__pycache__ --exclude='*.pem' --exclude='*.jks' \
      -cf - "$name" | tar -C "$root" -xf -
    jq --arg v ">=$semver" '.requires.rift = $v' "$SRC_DIR/$name/template.json" \
      > "$root/$name/template.json"
  done <<<"$names"

  tar -czf "$out" -C "$stage" "rift-templates-$version"
  ( cd "$(dirname "$out")" && sha256_of "$(basename "$out")" > "$(basename "$out").sha256" )

  echo "[ok] wrote $out"
  echo "[ok] wrote $out.sha256"
}

# Prove the packager end-to-end: every template is packaged with its files intact, smoke.sh stays
# executable, requires.rift is stamped from a `v`-prefixed tag, and the checksum verifies.
self_test() {
  local work ver="v9.9.9-selftest"
  work="$(mktemp -d)"; WORK_DIR="$work"

  package "$ver" "$work/out.tar.gz"
  [ -f "$work/out.tar.gz" ] || fail "tarball not produced"
  [ -f "$work/out.tar.gz.sha256" ] || fail "checksum not produced"
  local want got
  want="$(awk '{print $1}' "$work/out.tar.gz.sha256")"
  got="$(sha256_of "$work/out.tar.gz" | awk '{print $1}')"
  [ "$want" = "$got" ] || fail "checksum does not match the tarball"

  tar -xzf "$work/out.tar.gz" -C "$work"
  local extracted="$work/rift-templates-$ver"
  [ -f "$extracted/README.md" ] || fail "README.md missing from tarball"

  local name count=0
  while IFS= read -r name; do
    count=$((count + 1))
    local pkg="$extracted/$name"
    [ -d "$pkg" ] || fail "$name missing from tarball"
    local entry
    entry="$(jq -r '.entrypoint' "$pkg/template.json")"
    [ -f "$pkg/$entry" ] || fail "$name: entrypoint $entry missing from tarball"
    [ -x "$pkg/smoke.sh" ] || fail "$name: smoke.sh missing or not executable in tarball"
    local stamped
    stamped="$(jq -r '.requires.rift' "$pkg/template.json")"
    [ "$stamped" = ">=9.9.9-selftest" ] || fail "$name: requires.rift not stamped (got '$stamped')"
    # Everything else is byte-identical to the source.
    local disk packaged
    disk="$(cd "$SRC_DIR/$name" && find . -type f ! -name template.json ! -path '*/.venv/*' \
      ! -path '*/__pycache__/*' ! -name '*.pem' ! -name '*.jks' | sort)"
    packaged="$(cd "$pkg" && find . -type f ! -name template.json | sort)"
    [ "$disk" = "$packaged" ] || fail "$name: file list drift (disk vs tarball)"
    while IFS= read -r f; do
      cmp -s "$SRC_DIR/$name/$f" "$pkg/$f" || fail "$name: $f differs from the source"
    done <<<"$packaged"
  done <<<"$(template_dirs)"
  [ "$count" -gt 0 ] || fail "no template packaged"

  echo "[ok] self-test passed ($count template(s), requires.rift stamped, checksum verified)"
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
