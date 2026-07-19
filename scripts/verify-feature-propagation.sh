#!/usr/bin/env bash
#
# Feature-propagation gate (issue #777).
#
# `rift-mock-core` is the engine library, but nobody runs it directly: users run the `rift` binary
# (`rift-http-proxy`) or embed the C-ABI (`rift-ffi`). Both of those depend on it with
# `default-features = false` — deliberately, so a `cdylib` never inherits an allocator — which means
# a feature that is default-ON for `rift-mock-core` reaches **nothing users run** unless the
# dependent explicitly forwards it.
#
# That is exactly how #777 shipped: `quamina-matching` was default-on for `rift-mock-core`, its
# tests ran there and passed, and the dimension was compiled out of the binary and the C-ABI. CI was
# green the whole time, because the tests live in the one crate where the feature *was* enabled.
#
# This gate asserts the invariant that would have caught it: every default feature of
# `rift-mock-core` is forwarded by every crate that takes it with `default-features = false`, and is
# itself default-on there — so "on by default in the library" means "on by default in what ships".
#
# Deliberate non-forwards go in DELIBERATELY_NOT_FORWARDED below, with a reason. Adding one is a
# decision, not a workaround: it means the shipped artifacts intentionally differ from the library.
#
# Usage:
#   scripts/verify-feature-propagation.sh              # check the workspace (exit 1 on any gap)
#   scripts/verify-feature-propagation.sh --self-test  # prove the checker flags a planted gap
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="${MANIFEST:-$repo_root/Cargo.toml}"

# Features intentionally NOT forwarded, as "crate:feature=reason". Empty today.
# (`mimalloc` is NOT here: it is a `rift-http-proxy` feature, not a `rift-mock-core` one, so it is
# outside this invariant entirely.)
DELIBERATELY_NOT_FORWARDED=()

check() {
  local manifest="$1"
  python3 - "$manifest" "${DELIBERATELY_NOT_FORWARDED[@]:-}" <<'PY'
import json, subprocess, sys

manifest = sys.argv[1]
exempt = set(a for a in sys.argv[2:] if a)

md = json.loads(subprocess.run(
    ["cargo", "metadata", "--no-deps", "--format-version", "1", "--manifest-path", manifest],
    capture_output=True, text=True, check=True).stdout)

pkgs = {p["name"]: p for p in md["packages"]}
LIB = "rift-mock-core"
if LIB not in pkgs:
    sys.exit(f"{LIB} not found in workspace metadata — has the crate been renamed?")

lib_defaults = [f for f in pkgs[LIB]["features"].get("default", [])]

# Every workspace crate that depends on the library with default-features = false must forward.
dependents = []
for name, p in pkgs.items():
    for d in p["dependencies"]:
        if d["name"] == LIB and d["kind"] is None and not d.get("uses_default_features", True):
            dependents.append(name)
            break

failures = []
for feat in lib_defaults:
    for dep in sorted(set(dependents)):
        if f"{dep}:{feat}" in {e.split('=')[0] for e in exempt}:
            continue
        feats = pkgs[dep]["features"]
        target = f"{LIB}/{feat}"
        forwarding = [k for k, v in feats.items() if target in v]
        if not forwarding:
            failures.append(
                f"{dep}: does not forward {LIB}'s default feature '{feat}'.\n"
                f"    Add:  {feat} = [\"{target}\"]   (and put '{feat}' in {dep}'s default = [...])\n"
                f"    Effect today: '{feat}' is compiled OUT of {dep} — it reaches nothing users run.")
            continue
        # Forwarded, but is it on by default here too?
        if not any(f in feats.get("default", []) for f in forwarding):
            failures.append(
                f"{dep}: forwards '{feat}' via {forwarding} but none of those are in its "
                f"default = [...], so it is off unless a caller opts in.")

if failures:
    print("Feature-propagation gate FAILED:\n", file=sys.stderr)
    for f in failures:
        print(f"  - {f}\n", file=sys.stderr)
    print(f"{LIB} defaults: {lib_defaults}", file=sys.stderr)
    print(f"dependents taking it with default-features=false: {sorted(set(dependents))}", file=sys.stderr)
    sys.exit(1)

print(f"Feature propagation OK: {LIB} defaults {lib_defaults} "
      f"forwarded and default-on in {sorted(set(dependents))}.")
PY
}

self_test() {
  # Prove the checker is not a no-op: copy the workspace manifests, strip one passthrough, and
  # require a failure. Copying only the manifests keeps this fast (no source tree copy needed —
  # `cargo metadata --no-deps` reads manifests only).
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  ( cd "$repo_root" && git ls-files '*Cargo.toml' | while read -r f; do
      mkdir -p "$tmp/$(dirname "$f")" && cp "$f" "$tmp/$f"
    done )
  # Planted gap: remove rift-http-proxy's javascript passthrough.
  python3 - "$tmp/crates/rift-http-proxy/Cargo.toml" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p).read()
t = re.sub(r'^javascript = \[.*?\]\n', '', t, count=1, flags=re.M)
open(p, 'w').write(t)
PY
  if MANIFEST="$tmp/Cargo.toml" check "$tmp/Cargo.toml" >/dev/null 2>&1; then
    echo "SELF-TEST FAILED: the checker passed a manifest with a removed passthrough" >&2
    exit 1
  fi
  echo "Self-test OK: the checker rejects a removed feature passthrough."
}

if [[ "${1:-}" == "--self-test" ]]; then
  self_test
else
  check "$MANIFEST"
fi
