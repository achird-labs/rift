#!/usr/bin/env bash
# Fails when the committed animated SVGs in docs/assets differ from what the scene scripts build,
# or when one is not valid XML, has no aria-label, or is over 40 KB.
#
#   scripts/visuals/check.sh              check docs/assets against a fresh build
#   scripts/visuals/check.sh --self-test  prove the check catches a stale or broken file
#
# Stdlib Python only. The logo PNGs (scripts/visuals/logo.py, Pillow) are not checked here.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SCENES="hero response-cycle proxy-modes matching connection-path scenarios"
MAX_BYTES=40960

# compare <built dir> <committed dir>: 0 when every scene matches and is well-formed
compare() {
  local built="$1" committed="$2" fail=0
  for name in $SCENES; do
    local want="$built/$name.svg" have="$committed/$name.svg"
    if [ ! -f "$have" ]; then
      echo "missing: docs/assets/$name.svg (run: python3 scripts/visuals/build.py $name)"
      fail=1
      continue
    fi
    if ! cmp -s "$want" "$have"; then
      echo "stale: docs/assets/$name.svg differs from its scene script (run: python3 scripts/visuals/build.py $name)"
      diff -u "$have" "$want" | head -20 || true
      fail=1
    fi
    if ! python3 -I - "$have" "$MAX_BYTES" <<'PY'
import os, sys
import xml.etree.ElementTree as ET
path, limit = sys.argv[1], int(sys.argv[2])
try:
    root = ET.parse(path).getroot()
except ET.ParseError as e:
    sys.exit(f"invalid XML: {path}: {e}")
if not root.get("aria-label"):
    sys.exit(f"no aria-label: {path}")
size = os.path.getsize(path)
if size > limit:
    sys.exit(f"too large: {path} is {size} bytes, over {limit}")
PY
    then
      fail=1
    fi
  done
  return $fail
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
python3 -I "$HERE/build.py" --out "$TMP/built" >/dev/null

if [ "${1:-}" = "--self-test" ]; then
  # A faithful copy passes; a stale copy and a malformed copy each fail.
  cp -R "$TMP/built" "$TMP/good"
  compare "$TMP/built" "$TMP/good" >/dev/null || { echo "self-test: an identical build was rejected"; exit 1; }
  cp -R "$TMP/built" "$TMP/stale"
  printf '<!-- edited -->\n' >> "$TMP/stale/matching.svg"
  if compare "$TMP/built" "$TMP/stale" >/dev/null; then echo "self-test: a stale SVG was accepted"; exit 1; fi
  cp -R "$TMP/built" "$TMP/broken"
  printf '<svg xmlns="http://www.w3.org/2000/svg"><g></svg>\n' > "$TMP/broken/hero.svg"
  mkdir "$TMP/built2" && cp "$TMP/built/"*.svg "$TMP/built2/" && cp "$TMP/broken/hero.svg" "$TMP/built2/hero.svg"
  if compare "$TMP/built2" "$TMP/broken" >/dev/null 2>&1; then echo "self-test: malformed XML was accepted"; exit 1; fi
  echo "self-test ok"
  exit 0
fi

compare "$TMP/built" "$ROOT/docs/assets"
echo "docs/assets SVGs match their scene scripts"
