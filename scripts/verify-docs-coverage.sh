#!/usr/bin/env bash
#
# Docs reference-coverage gate (issue #280, Section E).
#
# Extracts the user-facing CLI reference surface from source — `rift-http-proxy` flags (top-level
# and subcommand), environment variables, and subcommands — and fails if any of them is not
# mentioned in the docs. This keeps docs/configuration/cli.md from silently drifting behind the
# code: add a flag or env var without documenting it and CI goes red.
#
# It deliberately covers only what can be extracted *reliably* from source. The admin router is
# hand-rolled dispatch (segment matching, not a route table), so route coverage is not gated
# here — see the issue for the example-execution gate follow-up.
#
# Usage:
#   scripts/verify-docs-coverage.sh              # check the repo (exit 1 on any gap)
#   scripts/verify-docs-coverage.sh --self-test  # prove the checker flags a planted gap
#
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Overridable so the self-test can point the extractor at fixtures.
SERVER_RS="${SERVER_RS:-$repo_root/crates/rift-http-proxy/src/server.rs}"
DOCS_DIR="${DOCS_DIR:-$repo_root/docs}"
# Directories scanned for `std::env::var("RIFT_…")` reads that aren't clap `env=` attrs.
ENV_SCAN_DIR="${ENV_SCAN_DIR:-$repo_root/crates}"

# Build-time stamp vars (set by build.rs / CI, not user runtime config) are excluded.
BUILD_STAMP_ENV='^(RIFT_COMMIT|RIFT_BUILT_AT)$'

# --- extractors -------------------------------------------------------------

# Long flags from clap `#[arg(long …)]` attributes. Handles: multi-line attribute blocks,
# an explicit `long = "renamed"` (the real flag differs from the field name), and doc-comment /
# stacked-attribute lines between the attribute and the field. A bare `long` derives the flag
# from the field name (`snake_case` → `--kebab-case`).
#
# Assumes the attribute and the field are on separate lines — true for rustfmt-clean code, which
# CI enforces via `cargo fmt --all -- --check`. A hand-written same-line `#[arg(long)] pub f: T,`
# would be missed; keep attributes on their own line (rustfmt does this automatically).
extract_flags() {
  awk '
    function reset() { in_attr = 0; have_long = 0; explicit = ""; armed = 0 }
    BEGIN { reset() }

    # Enter an #[arg( … )] attribute block (may span several lines).
    !in_attr && /#\[[[:space:]]*arg[[:space:]]*\(/ { in_attr = 1; have_long = 0; explicit = "" }

    in_attr {
      if (match($0, /long[[:space:]]*=[[:space:]]*"[^"]+"/)) {
        s = substr($0, RSTART, RLENGTH)
        sub(/long[[:space:]]*=[[:space:]]*"/, "", s); sub(/".*/, "", s)
        explicit = s; have_long = 1
      } else if ($0 ~ /(\(|,|[[:space:]])long([[:space:]]*(,|\)|$))/) {
        have_long = 1
      }
      if ($0 ~ /\)[[:space:]]*\]/) { in_attr = 0; if (have_long) armed = 1 }
      next
    }

    # After the attribute closes, skip doc comments / other attributes / blank lines…
    armed && /^[[:space:]]*(\/\/|#\[|$)/ { next }

    # …until the field declaration; `pub` is optional (subcommand fields omit it).
    armed && /^[[:space:]]*(pub[[:space:]]+)?[a-z_][a-z0-9_]*[[:space:]]*:/ {
      if (explicit != "") {
        print "--" explicit
      } else {
        f = $0; sub(/^[[:space:]]*(pub[[:space:]]+)?/, "", f); sub(/[[:space:]]*:.*/, "", f)
        gsub(/_/, "-", f); print "--" f
      }
      armed = 0; next
    }

    # Any other line cancels a pending field (e.g. end of struct).
    armed { armed = 0 }
  ' "$SERVER_RS" | sort -u
}

# Subcommand variant names from `enum Commands { … }` → lowercased.
extract_subcommands() {
  awk '
    /enum[[:space:]]+Commands[[:space:]]*\{/ { inblk = 1; next }
    inblk && /^\}/                           { inblk = 0 }
    inblk && /^[[:space:]]+[A-Z]/ {
      v = $1; gsub(/[^A-Za-z0-9].*/, "", v)
      if (v != "") print tolower(v)
    }
  ' "$SERVER_RS" | sort -u
}

# App env vars (MB_*, RIFT_*, NO_COLOR): clap `env = "…"` attrs plus `std::env::var*("…")` reads.
# Each source is guarded with `|| true` — "no matches" is a valid outcome for one source and must
# not abort the other under `set -e`/`pipefail` (a genuinely-broken extraction is caught by the
# non-empty guard in run_check, not by aborting here).
extract_env_vars() {
  {
    grep -oE 'env[[:space:]]*=[[:space:]]*"[A-Z0-9_]+"' "$SERVER_RS" 2>/dev/null \
      | grep -oE '"[A-Z0-9_]+"' | tr -d '"' || true
    grep -rhoE 'std::env::var(_os)?\("[A-Z0-9_]+"\)' "$ENV_SCAN_DIR" 2>/dev/null \
      | grep -oE '"[A-Z0-9_]+"' | tr -d '"' || true
  } | grep -E '^(MB_|RIFT_|NO_COLOR$)' | grep -vE "$BUILD_STAMP_ENV" | sort -u || true
}

# --- coverage check ---------------------------------------------------------

# True if the literal token appears under DOCS_DIR bounded by non-identifier chars, so that e.g.
# `--log` is NOT considered documented merely because `--loglevel` appears. Tokens are drawn from
# `[A-Za-z0-9_-]` (+ leading `--`), which carry no ERE metacharacters, so no escaping is needed.
documented() {
  grep -rqE -- "(^|[^A-Za-z0-9_-])$1([^A-Za-z0-9_-]|\$)" "$DOCS_DIR" 2>/dev/null
}

# True if a subcommand is shown as an invocation (`rift <sub>` / `rift-http-proxy <sub>`), rather
# than accepting any incidental backticked occurrence of a common word like `save`/`stop`.
subcommand_documented() {
  grep -rqE -- "rift(-http-proxy)?[[:space:]]+$1([^A-Za-z0-9]|\$)" "$DOCS_DIR" 2>/dev/null
}

run_check() {
  [[ -f "$SERVER_RS" ]] || { echo "ERROR: source not found: $SERVER_RS" >&2; return 2; }
  [[ -d "$DOCS_DIR"  ]] || { echo "ERROR: docs dir not found: $DOCS_DIR" >&2; return 2; }

  local missing=() flags=() envs=() subs=() count=0 line

  while IFS= read -r line; do [[ -n "$line" ]] && flags+=("$line"); done < <(extract_flags)
  while IFS= read -r line; do [[ -n "$line" ]] && envs+=("$line");  done < <(extract_env_vars)
  while IFS= read -r line; do [[ -n "$line" ]] && subs+=("$line");  done < <(extract_subcommands)

  # Non-empty guard: a silently-empty/collapsed extraction must never pass as "all covered".
  if [[ "${SKIP_MIN_GUARD:-}" != "1" ]]; then
    if (( ${#flags[@]} < 20 || ${#envs[@]} < 10 || ${#subs[@]} < 5 )); then
      echo "ERROR: extraction looks broken (flags=${#flags[@]} env=${#envs[@]} subs=${#subs[@]})." >&2
      echo "       Refusing to report success — the extractors likely need updating." >&2
      return 2
    fi
  fi

  for line in ${flags[@]+"${flags[@]}"} ${envs[@]+"${envs[@]}"}; do
    count=$((count + 1))
    documented "$line" || missing+=("$line")
  done
  for line in ${subs[@]+"${subs[@]}"}; do
    count=$((count + 1))
    subcommand_documented "$line" || missing+=("subcommand:$line")
  done

  if (( ${#missing[@]} > 0 )); then
    echo "Docs reference-coverage: ${#missing[@]}/${count} reference item(s) NOT documented under ${DOCS_DIR#"$repo_root"/}:" >&2
    printf '  - %s\n' "${missing[@]}" >&2
    return 1
  fi

  echo "Docs reference-coverage OK: ${#flags[@]} flags, ${#envs[@]} env vars, ${#subs[@]} subcommands all documented."
  return 0
}

# --- self-test --------------------------------------------------------------
#
# 1. Point the extractor at a fixture that exercises every flag-declaration style (single-line,
#    multi-line attribute, explicit `long =` rename, stacked attribute) plus an env var and a
#    subcommand — none of which the (empty) fixture docs mention — and assert the checker reports
#    exactly those. Proves the gate isn't a no-op and handles the real declaration styles.
# 2. Assert the non-empty guard itself fires (exit 2) when extraction is below threshold, so a
#    future regression that defangs the guard is caught here too.
self_test() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  cat >"$tmp/server.rs" <<'RS'
pub struct Cli {
    /// single-line attribute
    #[arg(long, env = "RIFT_SENTINEL_ENV")]
    pub sentinel_flag: bool,

    /// multi-line attribute block
    #[arg(
        long,
        value_name = "X"
    )]
    pub multiline_flag: bool,

    /// explicit rename: real flag != field name
    #[arg(long = "renamed-sentinel")]
    pub internal_name: bool,

    /// stacked attribute / doc line between #[arg] and the field
    #[arg(long)]
    #[serde(default)]
    pub stacked_flag: bool,
}
enum Commands {
    Sentinelcmd,
}
RS
  mkdir -p "$tmp/docs"
  echo "unrelated docs content" >"$tmp/docs/x.md"

  local out
  if out="$(SERVER_RS="$tmp/server.rs" DOCS_DIR="$tmp/docs" ENV_SCAN_DIR="$tmp" \
        SKIP_MIN_GUARD=1 run_check 2>&1)"; then
    echo "SELF-TEST FAILED: checker passed on a fixture with undocumented items." >&2
    return 1
  fi
  local expect
  for expect in "--sentinel-flag" "--multiline-flag" "--renamed-sentinel" "--stacked-flag" \
                "RIFT_SENTINEL_ENV" "subcommand:sentinelcmd"; do
    if ! grep -qF -- "$expect" <<<"$out"; then
      echo "SELF-TEST FAILED: expected the checker to flag '$expect'. Got:" >&2
      echo "$out" >&2
      return 1
    fi
  done

  # Guard must fire on a below-threshold extraction (no SKIP_MIN_GUARD).
  local rc=0
  out="$(SERVER_RS="$tmp/server.rs" DOCS_DIR="$tmp/docs" ENV_SCAN_DIR="$tmp" run_check 2>&1)" || rc=$?
  if (( rc != 2 )) || ! grep -qF "extraction looks broken" <<<"$out"; then
    echo "SELF-TEST FAILED: non-empty guard did not fire on a sparse extraction (rc=$rc). Got:" >&2
    echo "$out" >&2
    return 1
  fi

  echo "Self-test OK: checker flags planted undocumented flag/env/subcommand and the guard fires."
}

# --- main -------------------------------------------------------------------

case "${1:-}" in
  --self-test) self_test ;;
  "")          run_check ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 64 ;;
esac
