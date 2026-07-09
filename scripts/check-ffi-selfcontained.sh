#!/usr/bin/env bash
#
# Assert a `librift_ffi` cdylib is self-contained (issue #469): every library it dynamically links
# must be present on a *stock* host, i.e. a subset of a per-OS allowlist of system libraries. This
# catches leaks like the v0.11.3 build, which dynamically linked system LuaJIT
# (`libluajit-5.1.so.2` on Linux, `/opt/homebrew/opt/luajit/...` on macOS) and so failed to
# `dlopen` on any machine without LuaJIT installed — e.g. every stock GitHub ubuntu runner.
#
# `verify-ffi-cdylib.sh` proves the cdylib *exports* the C-ABI; this proves what it *imports*.
#
# Usage:
#   scripts/check-ffi-selfcontained.sh <path-to-librift_ffi.{so,dylib}>   # check a built cdylib
#   scripts/check-ffi-selfcontained.sh --self-test                        # prove the gate works
set -euo pipefail

# ELF DT_NEEDED names allowed on a stock Linux host: the C runtime, math/dl/rt/pthread, the libgcc
# unwinder, the dynamic loader — for both glibc and musl. Anything else (a scripting engine's
# native lib, a Homebrew/apt package) is a leak.
elf_allowed() {
  case "$1" in
    # glibc: versioned sonames
    libc.so.* | libm.so.* | libdl.so.* | librt.so.* | libpthread.so.* | libgcc_s.so.* | \
      ld-linux*.so.*) return 0 ;;
    # musl: the loader is the libc, and a musl-linked lib's NEEDED is a bare `libc.so`
    ld-musl-*.so.* | libc.musl-*.so.* | libc.so) return 0 ;;
    *) return 1 ;;
  esac
}

# Mach-O linked paths allowed on a stock macOS host: system frameworks and system libs only. A
# Homebrew (`/opt/homebrew`, `/usr/local`) or otherwise non-system path is a leak.
macho_allowed() {
  case "$1" in
    /System/Library/* | /usr/lib/*) return 0 ;;
    *) return 1 ;;
  esac
}

# Validate a newline-separated import list for a format ("elf"|"macho"); prints offenders and
# returns 1 if any import is outside the allowlist.
validate_imports() {
  local fmt="$1" imports="$2" bad=0 lib
  while IFS= read -r lib; do
    [ -n "$lib" ] || continue
    if [ "$fmt" = elf ]; then elf_allowed "$lib" || { echo "  [LEAK] $lib"; bad=1; }; fi
    if [ "$fmt" = macho ]; then macho_allowed "$lib" || { echo "  [LEAK] $lib"; bad=1; }; fi
  done <<<"$imports"
  return "$bad"
}

# Extract the dynamic imports of a real cdylib, one per line.
extract_imports() {
  local lib="$1"
  case "$lib" in
    *.so)
      if command -v readelf >/dev/null 2>&1; then
        readelf -d "$lib" | awk -F'[][]' '/\(NEEDED\)/ {print $2}'
      elif command -v objdump >/dev/null 2>&1; then
        objdump -p "$lib" | awk '/NEEDED/ {print $2}'
      else
        echo "[error] need readelf or objdump to inspect $lib" >&2
        return 2
      fi
      ;;
    *.dylib)
      # otool -L: line 1 is the `path:` header and the next line is the dylib's own install id
      # (LC_ID_DYLIB — a build path for an un-relocated artifact, not a real dependency). Skip the
      # header (NR>1) and any self-reference to librift_ffi; the rest are genuine dependencies.
      otool -L "$lib" | awk 'NR > 1 && $1 !~ /librift_ffi/ {print $1}'
      ;;
    *)
      echo "[error] unknown cdylib type (want .so or .dylib): $lib" >&2
      return 2
      ;;
  esac
}

check() {
  local lib="$1"
  [ -f "$lib" ] || { echo "[error] cdylib not found: $lib" >&2; exit 2; }
  local fmt
  case "$lib" in
    *.so) fmt=elf ;;
    *.dylib) fmt=macho ;;
    *) echo "[error] unknown cdylib type (want .so or .dylib): $lib" >&2; exit 2 ;;
  esac

  local imports
  imports="$(extract_imports "$lib")"
  # A real cdylib always links at least the C runtime; an empty list means the import extractor
  # silently produced nothing (e.g. a readelf/otool output-format drift) — fail loud rather than
  # let the gate pass vacuously.
  if [ -z "${imports//[[:space:]]/}" ]; then
    echo "[error] no dynamic imports extracted from $lib — the extractor may be broken" >&2
    exit 2
  fi
  echo "[info] $lib links:"
  echo "$imports" | sed 's/^/    /'

  if validate_imports "$fmt" "$imports"; then
    echo "[pass] $lib is self-contained (all imports are stock $fmt system libraries)"
  else
    echo "[fail] $lib links libraries absent from a stock host — see [LEAK] lines above." >&2
    echo "       A scripting/engine backend must be static/vendored, not dynamically linked (#469)." >&2
    exit 1
  fi
}

# Prove the gate is not a no-op: a clean import list passes; a list containing the exact v0.11.3
# LuaJIT leaks (and a Homebrew path) is rejected.
self_test() {
  local good_elf good_macho bad_elf bad_macho
  good_elf=$'libc.so.6\nlibm.so.6\nlibdl.so.2\nlibgcc_s.so.1\nld-linux-x86-64.so.2'
  good_macho=$'/System/Library/Frameworks/Security.framework/Versions/A/Security\n/usr/lib/libSystem.B.dylib'
  bad_elf=$'libc.so.6\nlibluajit-5.1.so.2'                       # the exact linux-x86_64 leak
  bad_macho=$'/opt/homebrew/opt/luajit/lib/libluajit-5.1.2.dylib'  # the exact darwin-aarch64 leak

  validate_imports elf   "$good_elf"   >/dev/null || { echo "[FAIL] self-test: rejected a clean ELF list" >&2; exit 1; }
  validate_imports macho "$good_macho" >/dev/null || { echo "[FAIL] self-test: rejected a clean Mach-O list" >&2; exit 1; }
  if validate_imports elf "$bad_elf" >/dev/null 2>&1; then
    echo "[FAIL] self-test: did NOT catch the ELF LuaJIT leak — the gate is a no-op" >&2; exit 1
  fi
  if validate_imports macho "$bad_macho" >/dev/null 2>&1; then
    echo "[FAIL] self-test: did NOT catch the Homebrew LuaJIT leak — the gate is a no-op" >&2; exit 1
  fi
  echo "[pass] self-test: clean imports accepted; the #469 LuaJIT leaks (ELF + Homebrew) rejected"
}

case "${1:-}" in
  --self-test) self_test ;;
  "" | -h | --help) echo "usage: $0 <path-to-librift_ffi.{so,dylib}> | --self-test" >&2; exit 64 ;;
  *) check "$1" ;;
esac
