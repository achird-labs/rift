#!/usr/bin/env bash
# Issue #205 C-ABI smoke test: build the rift-ffi cdylib for the host and assert it exports the
# full C-ABI symbol set. Runs locally and as a CI step before the per-platform release matrix.
#
# Any extra args are forwarded to `cargo build` (e.g. feature flags).
set -euo pipefail

SYMBOLS=(
  # v1 (issue #204)
  rift_start
  rift_create_imposter
  rift_replace_stubs
  rift_delete_all
  rift_recorded
  rift_free
  rift_stop
  # v2 (issue #343)
  rift_serve_admin
  rift_apply_config
  rift_delete_imposter
  rift_build_info
  rift_last_error
  # issue #423: stub-analysis warnings over the C-ABI
  rift_stub_warnings
  # issue #494: server-side verification (predicate-count + closest-match)
  rift_verify
  # issue #491: admin long tail — list/get imposters, stub surgery, clear/enable, scenarios
  rift_list_imposters
  rift_get_imposter
  rift_add_stub
  rift_get_stub
  rift_update_stub
  rift_delete_stub
  rift_clear_recorded
  rift_clear_proxy_recordings
  rift_set_imposter_enabled
  rift_scenarios
  rift_set_scenario_state
  rift_reset_scenarios
  # issue #591: queryable C-ABI contract version for SDK compatibility gating
  rift_abi_version
)

# Issue #344: the checked-in C header is the ABI's source of truth — assert it matches a fresh
# cbindgen run so it can never drift from the code. CI installs cbindgen; skip if absent locally.
HEADER="crates/rift-ffi/include/rift_ffi.h"
if command -v cbindgen >/dev/null 2>&1; then
  echo "[info] verifying $HEADER matches a fresh cbindgen run"
  tmp_header="$(mktemp)"
  trap 'rm -f "$tmp_header"' EXIT
  cbindgen --quiet --config crates/rift-ffi/cbindgen.toml --crate rift-ffi --output "$tmp_header" crates/rift-ffi
  if ! diff -u "$HEADER" "$tmp_header"; then
    echo "[fail] $HEADER is stale — regenerate: cbindgen --config crates/rift-ffi/cbindgen.toml --crate rift-ffi --output $HEADER crates/rift-ffi" >&2
    exit 1
  fi
  echo "[ok] $HEADER is up to date"
else
  echo "[warn] cbindgen not installed — skipping header diff (install: cargo install cbindgen)"
fi

echo "[info] building librift_ffi cdylib (release)..."
cargo build -p rift-ffi --release "$@"

case "$(uname -s)" in
  Darwin)
    lib="target/release/librift_ffi.dylib"
    list_symbols() { nm -gU "$1"; }
    ;;
  Linux)
    lib="target/release/librift_ffi.so"
    list_symbols() { nm -D --defined-only "$1"; }
    ;;
  *)
    echo "[error] unsupported host OS '$(uname -s)' for symbol check" >&2
    exit 1
    ;;
esac

if [ ! -f "$lib" ]; then
  echo "[error] cdylib not found at $lib" >&2
  exit 1
fi

echo "[info] checking exported symbols in $lib"
exported="$(list_symbols "$lib")"
missing=0
for sym in "${SYMBOLS[@]}"; do
  # macOS prefixes C symbols with a leading underscore; match either form.
  if printf '%s\n' "$exported" | grep -qE " _?${sym}\$"; then
    echo "  [ok]      $sym"
  else
    echo "  [MISSING] $sym"
    missing=1
  fi
done

if [ "$missing" -ne 0 ]; then
  echo "[fail] librift_ffi is missing one or more C-ABI symbols" >&2
  exit 1
fi
echo "[pass] all ${#SYMBOLS[@]} C-ABI symbols exported by $lib"

# Issue #469: exporting the C-ABI is necessary but not sufficient — the cdylib must also be
# self-contained (no system-LuaJIT / Homebrew leaks) so it can dlopen on stock hosts.
"$(dirname "${BASH_SOURCE[0]}")/check-ffi-selfcontained.sh" "$lib"
