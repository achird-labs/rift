#!/usr/bin/env bash
#
# Emit ffi-manifest.json (issue #459): a per-platform map of the librift_ffi cdylib assets in a
# release — `{version, abi, artifacts: [{platform, file, sha256, url}]}`. SDK consumers (rift-java
# natives packaging, rift-go's fetcher/loader, spawn-transport binary downloads) read this instead
# of hardcoding release-URL patterns.
#
# The manifest is sourced entirely from the per-platform `librift_ffi-<classifier>.<ext>.sha256`
# assets the release workflow already produces (issue #205): the `file` is the checksum file's
# basename minus `.sha256`, the `sha256` is its first field, and the `platform` is the classifier
# parsed from the filename. Platforms with no cdylib (musl, which can't produce one) have no such
# asset and are simply absent — the manifest tracks whatever cdylibs the release actually shipped.
#
# Download URLs are built from `$GITHUB_SERVER_URL/$GITHUB_REPOSITORY` (both auto-set by GitHub
# Actions), so nothing about the repo is hardcoded here.
#
# Usage:
#   scripts/gen-ffi-manifest.sh <version> <search-dir> [output-file]  # write the manifest
#   scripts/gen-ffi-manifest.sh --self-test                          # prove the generator works
set -euo pipefail

fail() { echo "[FAIL] $*" >&2; exit 1; }

# Build the manifest from every `librift_ffi-*.sha256` under <search-dir> (recursively, matching
# the flat-per-target layout `actions/download-artifact` produces).
generate() {
  local version="$1" search_dir="$2" out="${3:-ffi-manifest.json}"
  [ -n "$version" ] || fail "version argument is required"
  [ -d "$search_dir" ] || fail "search dir not found: $search_dir"

  # Resolved at call time (not once at script top level) so a per-invocation
  # GITHUB_REPOSITORY/GITHUB_SERVER_URL override reaches this function — the hermetic
  # --self-test relies on that. In CI these carry the ambient repo values, so the real
  # release-manifest output is unchanged. (Top-level resolution silently ignored the
  # self-test's override and only "passed" while the repo slug matched the hardcoded
  # expectation — it broke the moment the repo moved orgs.)
  local server_url="${GITHUB_SERVER_URL:-https://github.com}"
  local repo="${GITHUB_REPOSITORY:-achird-labs/rift}"
  local base_url="$server_url/$repo/releases/download/$version"
  local artifacts="[]"
  local count=0

  # Deterministic order: sort by checksum path so the manifest is reproducible.
  local sha_file file sha platform
  while IFS= read -r sha_file; do
    file="$(basename "${sha_file%.sha256}")"          # librift_ffi-<platform>.<ext>
    sha="$(awk 'NR==1 {print $1}' "$sha_file")"
    [ -n "$sha" ] || fail "empty sha256 in $sha_file"
    platform="${file#librift_ffi-}"                   # <platform>.<ext>
    platform="${platform%.*}"                         # <platform>
    [ -n "$platform" ] || fail "could not parse platform from $file"
    artifacts="$(jq \
      --arg platform "$platform" \
      --arg file "$file" \
      --arg sha "$sha" \
      --arg url "$base_url/$file" \
      '. += [{platform: $platform, file: $file, sha256: $sha, url: $url}]' <<<"$artifacts")"
    count=$((count + 1))
  done < <(find "$search_dir" -type f -name 'librift_ffi-*.sha256' | sort)

  [ "$count" -gt 0 ] || fail "no librift_ffi-*.sha256 assets found under $search_dir"

  jq -n --arg version "$version" --argjson artifacts "$artifacts" \
    '{version: $version, abi: "v2", artifacts: $artifacts}' >"$out"
  echo "[ok] wrote $out with $count artifact(s)"
}

# Plant fake per-platform checksum assets, generate, and assert the manifest's shape and content —
# including that an assetless directory is a hard failure, so the gate can't pass vacuously.
self_test() {
  local tmp; tmp="$(mktemp -d)"
  # Expand $tmp into the trap now — it is function-local and out of scope when EXIT fires.
  trap "rm -rf '$tmp'" EXIT

  mkdir -p "$tmp/x86_64-unknown-linux-gnu" "$tmp/aarch64-apple-darwin" "$tmp/x86_64-pc-windows-msvc"
  printf 'aaaa1111  librift_ffi-linux-x86_64.so\n' \
    >"$tmp/x86_64-unknown-linux-gnu/librift_ffi-linux-x86_64.so.sha256"
  printf 'bbbb2222  librift_ffi-darwin-aarch64.dylib\n' \
    >"$tmp/aarch64-apple-darwin/librift_ffi-darwin-aarch64.dylib.sha256"
  printf 'cccc3333  librift_ffi-windows-x86_64.dll\n' \
    >"$tmp/x86_64-pc-windows-msvc/librift_ffi-windows-x86_64.dll.sha256"

  # Inject a deliberately-synthetic repo so the self-test is hermetic and immune to the real
  # repo's slug (e.g. an org rename/transfer) — it asserts the generator threads
  # GITHUB_REPOSITORY into the URL, nothing about which org actually owns the repo.
  local out="$tmp/ffi-manifest.json"
  GITHUB_SERVER_URL="https://example.com" GITHUB_REPOSITORY="example-org/example-repo" \
    generate "v9.9.9" "$tmp" "$out"

  jq -e . "$out" >/dev/null || fail "output is not valid JSON"
  [ "$(jq -r .abi "$out")" = "v2" ] || fail "abi must be v2"
  [ "$(jq -r .version "$out")" = "v9.9.9" ] || fail "version not propagated"
  [ "$(jq '.artifacts | length' "$out")" -eq 3 ] || fail "expected 3 artifacts"

  local lx
  lx="$(jq -c '.artifacts[] | select(.platform == "linux-x86_64")' "$out")"
  [ -n "$lx" ] || fail "missing linux-x86_64 entry"
  [ "$(jq -r .file <<<"$lx")" = "librift_ffi-linux-x86_64.so" ] || fail "linux file wrong"
  [ "$(jq -r .sha256 <<<"$lx")" = "aaaa1111" ] || fail "linux sha256 wrong"
  [ "$(jq -r .url <<<"$lx")" = \
    "https://example.com/example-org/example-repo/releases/download/v9.9.9/librift_ffi-linux-x86_64.so" ] \
    || fail "linux url wrong"

  jq -e '.artifacts[] | select(.platform == "darwin-aarch64" and .sha256 == "bbbb2222"
    and .file == "librift_ffi-darwin-aarch64.dylib")' "$out" >/dev/null \
    || fail "darwin-aarch64 entry wrong"
  jq -e '.artifacts[] | select(.platform == "windows-x86_64" and .sha256 == "cccc3333"
    and .file == "librift_ffi-windows-x86_64.dll")' "$out" >/dev/null \
    || fail "windows-x86_64 entry wrong"

  # Negative case: an assetless directory must fail, proving the gate isn't a no-op.
  local empty; empty="$(mktemp -d)"
  if (generate "v9.9.9" "$empty" "$empty/out.json") 2>/dev/null; then
    rm -rf "$empty"; fail "generator must exit non-zero when no FFI assets are present"
  fi
  rm -rf "$empty"

  echo "[pass] gen-ffi-manifest self-test"
}

case "${1:-}" in
  --self-test) self_test ;;
  "" | -h | --help) echo "usage: $0 <version> <search-dir> [output-file] | --self-test" >&2; exit 64 ;;
  *) generate "$@" ;;
esac
