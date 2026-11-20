#!/usr/bin/env bash
#
# Package rift-mock-wit-<version>.tar.gz (issue #1797): the `rift:mock` WIT contract as a release
# asset, so a guest author or another host does not need to clone Rift. It unpacks to
#
#   rift-mock-wit-<version>/
#     README.md                     what is in it and where the semantics are specified
#     rift-mock.wit                 the contract (from crates/rift-mock-core/wit/)
#     conformance/                  the conformance guest: Cargo.toml, Cargo.lock, src/, script.json
#     conformance/rift_mock_conformance.wasm   the guest, built, when <guest.wasm> is given
#
# The guest's Cargo.toml reads `../rift-mock.wit`, so it builds from the unpacked tree as it does in
# the repository. A `.sha256` sidecar is written next to the tarball.
#
# Usage:
#   scripts/gen-rift-mock-wit.sh <version> [output.tar.gz] [guest.wasm]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WIT_DIR="$REPO_ROOT/crates/rift-mock-core/wit"
STAGE_DIR=""
cleanup() {
  [ -n "$STAGE_DIR" ] && rm -rf "$STAGE_DIR"
  return 0
}
trap cleanup EXIT

fail() { echo "[FAIL] $*" >&2; exit 1; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"
  else shasum -a 256 "$1"
  fi
}

version="${1:-}"
[ -n "$version" ] || fail "usage: $0 <version> [output.tar.gz] [guest.wasm]"
out="${2:-rift-mock-wit-$version.tar.gz}"
wasm="${3:-}"

for f in rift-mock.wit conformance/Cargo.toml conformance/Cargo.lock conformance/src/lib.rs \
  conformance/script.json; do
  [ -f "$WIT_DIR/$f" ] || fail "missing $WIT_DIR/$f"
done
[ -z "$wasm" ] || [ -f "$wasm" ] || fail "guest not found: $wasm"

mkdir -p "$(dirname "$out")"
out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
STAGE_DIR="$(mktemp -d)"
root="$STAGE_DIR/rift-mock-wit-$version"
mkdir -p "$root/conformance/src"

cp "$WIT_DIR/rift-mock.wit" "$root/"
cp "$WIT_DIR/conformance/Cargo.toml" "$WIT_DIR/conformance/Cargo.lock" \
  "$WIT_DIR/conformance/script.json" "$root/conformance/"
cp "$WIT_DIR/conformance/src/lib.rs" "$root/conformance/src/"
[ -z "$wasm" ] || cp "$wasm" "$root/conformance/rift_mock_conformance.wasm"

package_line="$(sed -n 's/^package \(.*\);$/\1/p' "$WIT_DIR/rift-mock.wit")"
cat > "$root/README.md" <<EOF
# rift:mock WIT contract — Rift $version

\`rift-mock.wit\` is \`$package_line\`, the world a \`_rift.script\` with \`engine: "wasm"\` runs.
Every function's semantics and the compatibility policy are specified in
https://github.com/achird-labs/rift/blob/$version/docs/features/wasm-wit.md

- Guest authors: point \`wit-bindgen\` at \`rift-mock.wit\` (world \`mock\`).
- Other hosts: implement \`rift:mock/state\` as specified, then run the conformance guest:
  instantiate \`conformance/rift_mock_conformance.wasm\`, call \`init\` with the text of
  \`conformance/script.json\`, then \`respond\` once on a fresh store. A \`respond\` decision with
  status 200 means every call matched; 422 lists each mismatch in its JSON body.
- To rebuild the guest: \`cargo build --release --target wasm32-wasip2\` in \`conformance/\`.
EOF

tar -czf "$out" -C "$STAGE_DIR" "rift-mock-wit-$version"
(cd "$(dirname "$out")" && sha256_of "$(basename "$out")" > "$(basename "$out").sha256")
echo "[OK] $out"
tar -tzf "$out"
