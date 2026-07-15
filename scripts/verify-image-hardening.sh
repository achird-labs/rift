#!/usr/bin/env bash
#
# Verify the container images stay lean and their publish pipeline stays attested (issue #664).
#
# The rift images ship one thing: the rift binary. Every extra OS package in them is CVE surface
# a downstream consumer has to chase (CVE-2025-10148 in Debian's curl is what prompted this), and
# curl was only ever there to serve the HEALTHCHECK. These checks keep it that way:
#
#   1. no image layer installs curl                     (it is gone; keep it gone)
#   2. every HEALTHCHECK uses the built-in probe        (exec form: scratch has no shell)
#   3. every FROM is digest-pinned                      (a floating tag is an unpinned dependency)
#   4. every runtime stage keeps a CA bundle            (hyper-rustls native-tokio needs it)
#   5. the static image installs no OS packages at all  (scratch/distroless, no apt/apk)
#   6. no compose healthcheck shells out to curl        (breaks the moment curl leaves the image)
#   7. published images get SBOM + provenance + cosign  (what image curators actually check)
#   8. dependabot watches the docker ecosystem          (so the pins above get bumped)
#
# Usage:
#   scripts/verify-image-hardening.sh              # check the repo
#   scripts/verify-image-hardening.sh --self-test  # prove each check flags a planted violation
#
# Overridable so the self-test can point the checks at a mutated copy of the tree.
ROOT="${ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"

set -uo pipefail

FAILURES=0

fail() {
  echo "  FAIL: $1" >&2
  FAILURES=$((FAILURES + 1))
}

ok() {
  echo "  ok: $1"
}

# Repo-relative path: two Dockerfiles share the basename `Dockerfile`, so a bare basename in a
# failure message doesn't say which image is broken.
rel() {
  echo "${1#"$ROOT"/}"
}

# Dockerfiles that build a published image. Discovered rather than listed so a new one is covered
# by default — an image nobody remembered to add to a list is exactly the one that rots.
dockerfiles() {
  find "$ROOT/crates" -name 'Dockerfile*' -type f 2>/dev/null | sort
}

# Anything that can define a healthcheck for a rift container: real compose files, and the compose
# snippets in the docs — a documented probe is copy-pasted into real deployments, so it rots the
# same way a real one does.
compose_files() {
  {
    find "$ROOT/docs" "$ROOT/tests" -name 'docker-compose*.yml' -type f 2>/dev/null
    grep -rl 'healthcheck:' "$ROOT/docs" --include='*.md' 2>/dev/null
  } | sort -u
}

# Strip comments so a check never matches an explanatory line about the thing it forbids.
uncommented() {
  sed -e 's/#.*$//' "$1"
}

# Only the lines belonging to the FINAL (runtime) stage of a Dockerfile. Builder stages are thrown
# away, so what they install or mention says nothing about the shipped image — a check that reads
# the whole file can be satisfied by a discarded stage.
runtime_stage() {
  uncommented "$1" | awk '
    /^FROM[[:space:]]/ { delete lines; n = 0; next }
    { lines[n++] = $0 }
    END { for (i = 0; i < n; i++) print lines[i] }
  '
}

# --- 1. no image layer installs curl ----------------------------------------
check_no_curl_installed() {
  echo "[1] no image layer installs curl"
  local f found=0
  for f in $(dockerfiles); do
    # Only package-manager lines matter: curl on the *runner* (workflows) is fine, curl baked into
    # an image layer is not.
    if uncommented "$f" | grep -nE '(apt-get|apt|apk|yum|dnf)[[:space:]]+(-[^[:space:]]+[[:space:]]+)*(install|add)' \
      | grep -qw 'curl'; then
      fail "$(rel "$f") installs curl"
      found=1
    fi
  done
  [ "$found" -eq 0 ] && ok "no Dockerfile installs curl"
}

# --- 2. HEALTHCHECK uses the built-in probe, in exec form -------------------
check_healthcheck_builtin() {
  echo "[2] HEALTHCHECK uses the built-in probe (exec form)"
  local f found=0
  for f in $(dockerfiles); do
    local hc
    hc=$(uncommented "$f" | grep -E '^[[:space:]]*(HEALTHCHECK|[[:space:]]+CMD)' || true)
    [ -z "$hc" ] && continue
    if echo "$hc" | grep -q 'curl'; then
      fail "$(rel "$f") HEALTHCHECK still shells out to curl"
      found=1
      continue
    fi
    # A CMD line belonging to a HEALTHCHECK must be exec form (a JSON array) and invoke the
    # binary's own probe: scratch has no shell to expand a string-form CMD.
    if echo "$hc" | grep -q 'HEALTHCHECK'; then
      if ! echo "$hc" | grep -qE '\[[[:space:]]*"[^"]*rift[^"]*"[[:space:]]*,[[:space:]]*"healthcheck"'; then
        fail "$(rel "$f") HEALTHCHECK is not exec-form \"rift\", \"healthcheck\""
        found=1
      fi
    fi
  done
  [ "$found" -eq 0 ] && ok "all HEALTHCHECKs use the built-in probe in exec form"
}

# --- 3. every FROM is digest-pinned -----------------------------------------
check_from_digest_pinned() {
  echo "[3] every FROM is digest-pinned"
  local f found=0
  for f in $(dockerfiles); do
    while IFS= read -r line; do
      [ -z "$line" ] && continue
      # `FROM scratch` is the empty image — it has no digest to pin, by definition.
      echo "$line" | grep -qE '^FROM[[:space:]]+scratch([[:space:]]|$)' && continue
      if ! echo "$line" | grep -q '@sha256:'; then
        fail "$(rel "$f"): unpinned base -> $line"
        found=1
      fi
    done < <(uncommented "$f" | grep -E '^FROM[[:space:]]' || true)
  done
  [ "$found" -eq 0 ] && ok "all FROM lines are digest-pinned (or scratch)"
}

# --- 4. runtime stages keep a CA bundle -------------------------------------
check_ca_certificates() {
  echo "[4] runtime stages keep a CA bundle"
  # hyper-rustls is built with `native-tokio`, so it loads the OS trust store at runtime. Drop the
  # CA bundle and HTTPS upstream proxying breaks silently — the exact failure this check exists for.
  # Scoped to the runtime stage: a builder stage that still mentions ca-certificates must not
  # satisfy this on behalf of a runtime stage that dropped it.
  local f found=0
  for f in $(dockerfiles); do
    if ! runtime_stage "$f" | grep -qE 'ca-certificates'; then
      fail "$(rel "$f") runtime stage has no CA bundle (breaks HTTPS upstreams)"
      found=1
    fi
  done
  [ "$found" -eq 0 ] && ok "every image's runtime stage keeps a CA bundle"
}

# --- 5. the static image installs no OS packages ----------------------------
check_static_has_no_os_packages() {
  echo "[5] the static image installs no OS packages"
  local f="$ROOT/crates/rift-http-proxy/Dockerfile.static"
  if [ ! -f "$f" ]; then
    fail "crates/rift-http-proxy/Dockerfile.static is missing"
    return
  fi
  # The whole point of the flavor: no package manager ran, so a scanner finds no OS packages.
  local final_from
  final_from=$(uncommented "$f" | grep -E '^FROM[[:space:]]' | tail -1)
  if ! echo "$final_from" | grep -qE '^FROM[[:space:]]+(scratch|gcr\.io/distroless)'; then
    fail "Dockerfile.static runtime stage is not scratch/distroless -> $final_from"
    return
  fi
  if runtime_stage "$f" | grep -qE '(apt-get|apk|yum|dnf)[[:space:]]+(install|add|update)'; then
    fail "Dockerfile.static runtime stage installs OS packages"
    return
  fi
  ok "static runtime stage is scratch/distroless with no package installs"
}

# --- 6. no compose healthcheck shells out to curl ---------------------------
check_compose_probes() {
  echo "[6] no compose healthcheck shells out to curl"
  local f found=0
  for f in $(compose_files); do
    if grep -nE '^[[:space:]]*test:' "$f" | grep -q 'curl'; then
      fail "$(rel "$f") healthchecks with curl (image no longer ships it)"
      found=1
    fi
  done
  [ "$found" -eq 0 ] && ok "all compose healthchecks use the built-in probe"
}

# --- 7. published images get SBOM + provenance + cosign ---------------------
check_publish_attestations() {
  echo "[7] published images get SBOM + provenance + a signature"
  local f="$ROOT/.github/workflows/docker-publish.yml"
  if [ ! -f "$f" ]; then
    fail "docker-publish.yml is missing"
    return
  fi
  # `provenance: false` is the old setting this issue reverses.
  if grep -qE '^[[:space:]]*provenance:[[:space:]]*false' "$f"; then
    fail "docker-publish.yml still sets provenance: false"
  else
    ok "no provenance: false"
  fi

  # Counted, not just present-somewhere. A file-wide `grep -q` is satisfied by the OTHER jobs'
  # settings, so adding a fourth image job with no attestations would pass — which is exactly the
  # copy-paste regression this check exists to stop. Every build step must carry both attestations.
  local builds sbom prov
  builds=$(grep -cE '^[[:space:]]*uses:[[:space:]]*docker/build-push-action' "$f" || true)
  sbom=$(grep -cE '^[[:space:]]*sbom:[[:space:]]*true' "$f" || true)
  prov=$(grep -cE '^[[:space:]]*provenance:[[:space:]]*mode=max' "$f" || true)

  [ "$builds" -gt 0 ] \
    || fail "docker-publish.yml has no build-push-action steps — this check would pass vacuously"
  [ "$sbom" -ge "$builds" ] \
    && ok "sbom: true on all $builds build step(s)" \
    || fail "only $sbom of $builds build step(s) request an SBOM"
  [ "$prov" -ge "$builds" ] \
    && ok "provenance: mode=max on all $builds build step(s)" \
    || fail "only $prov of $builds build step(s) request max provenance"

  # Every job that pushes must also sign, and keyless signing needs its own OIDC token.
  local pushers signers tokens
  pushers=$(grep -cE '^[[:space:]]*uses:[[:space:]]*docker/login-action' "$f" || true)
  signers=$(grep -cE '^[[:space:]]*uses:[[:space:]]*sigstore/cosign-installer' "$f" || true)
  tokens=$(grep -cE '^[[:space:]]*id-token:[[:space:]]*write' "$f" || true)
  [ "$signers" -ge "$pushers" ] \
    && ok "cosign present in all $pushers publishing job(s)" \
    || fail "only $signers of $pushers publishing job(s) install cosign"
  [ "$tokens" -ge "$pushers" ] \
    && ok "id-token: write in all $pushers publishing job(s)" \
    || fail "only $tokens of $pushers publishing job(s) grant id-token: write (cosign keyless needs it)"
}

# --- 8. dependabot watches the docker ecosystem -----------------------------
check_dependabot_docker() {
  echo "[8] dependabot watches the docker ecosystem"
  local f="$ROOT/.github/dependabot.yml"
  if [ ! -f "$f" ]; then
    fail ".github/dependabot.yml is missing (digest pins would never get bumped)"
    return
  fi
  if grep -qE 'package-ecosystem:[[:space:]]*"?docker"?' "$f"; then
    ok "dependabot covers docker"
  else
    fail "dependabot.yml does not cover the docker ecosystem"
  fi
}

# A gate that inspects nothing passes everything. If discovery comes back empty — wrong ROOT, a
# renamed directory, a `find` quirk — every per-file check below would loop zero times and report
# `ok`. Fail closed instead: no inputs is a broken checker, not a clean repo.
check_discovery_is_not_empty() {
  echo "[0] the checker actually found something to check"
  local n_docker n_compose
  n_docker=$(dockerfiles | grep -c . || true)
  n_compose=$(compose_files | grep -c . || true)
  [ "$n_docker" -ge 2 ] \
    && ok "found $n_docker Dockerfiles" \
    || fail "found only $n_docker Dockerfile(s) under $ROOT/crates — checks 1-4 would pass vacuously"
  [ "$n_compose" -ge 2 ] \
    && ok "found $n_compose compose sources" \
    || fail "found only $n_compose compose source(s) — check 6 would pass vacuously"
}

run_checks() {
  FAILURES=0
  check_discovery_is_not_empty
  check_no_curl_installed
  check_healthcheck_builtin
  check_from_digest_pinned
  check_ca_certificates
  check_static_has_no_os_packages
  check_compose_probes
  check_publish_attestations
  check_dependabot_docker
  return $FAILURES
}

# --- self-test --------------------------------------------------------------
# A checker that cannot fail is a checker that proves nothing. Plant one violation per check in a
# throwaway copy of the tree and assert the check actually catches it.
self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  local planted=0 caught=0 broken=0

  # Copy only what the checks read; copying the repo would drag in target/. `cp --parents` is a GNU
  # extension, so build the tree explicitly rather than relying on it.
  seed_tree() {
    rm -rf "$tmp/tree"
    mkdir -p "$tmp/tree/crates/rift-http-proxy" "$tmp/tree/crates/rift-lint" \
             "$tmp/tree/docs" "$tmp/tree/.github/workflows"
    cp "$ROOT"/crates/rift-http-proxy/Dockerfile* "$tmp/tree/crates/rift-http-proxy/"
    cp "$ROOT"/crates/rift-lint/Dockerfile* "$tmp/tree/crates/rift-lint/"
    cp -R "$ROOT/docs/demo" "$tmp/tree/docs/demo"
    cp -R "$ROOT/tests" "$tmp/tree/tests"
    cp "$ROOT/.github/workflows/docker-publish.yml" "$tmp/tree/.github/workflows/"
    cp "$ROOT/.github/dependabot.yml" "$tmp/tree/.github/"
  }

  # A planted violation that doesn't actually change the file proves nothing, and would quietly
  # report "not caught" as though the *check* were broken. Assert the mutation bit.
  #
  # `expect` is the substring the RESPONSIBLE check must emit. Asserting only "run_checks failed"
  # is too weak: several plants trip more than one check (injecting `RUN apt-get install -y curl`
  # into the static image violates both the no-curl rule and the no-OS-packages rule), so the
  # intended check could rot into a no-op while the suite still went red on the other one — the one
  # failure mode a self-test exists to rule out.
  plant() {
    local name="$1" target="$2" expect="$3" mutate="$4"
    seed_tree
    # A plant may legitimately delete its target, so absence is a state, not an error.
    fingerprint() {
      [ -f "$1" ] && cksum < "$1" || echo missing
    }

    local before after out
    before=$(fingerprint "$tmp/tree/$target")
    ( cd "$tmp/tree" && eval "$mutate" )
    after=$(fingerprint "$tmp/tree/$target")
    planted=$((planted + 1))
    if [ "$before" = "$after" ]; then
      echo "  SELF-TEST BROKEN: '$name' mutated nothing (stale pattern for $target)" >&2
      broken=$((broken + 1))
      return
    fi
    out=$(ROOT="$tmp/tree" run_checks 2>&1)
    if echo "$out" | grep -q "FAIL: .*${expect}"; then
      echo "  ok: '$name' is caught"
      caught=$((caught + 1))
    elif [ -n "$out" ] && ! ROOT="$tmp/tree" run_checks >/dev/null 2>&1; then
      echo "  SELF-TEST FAIL: '$name' tripped some other check, not the one under test" >&2
      echo "                  (expected a failure mentioning: ${expect})" >&2
    else
      echo "  SELF-TEST FAIL: '$name' was not caught" >&2
    fi
  }

  echo "self-test: planting one violation per check"
  plant "curl reinstalled" "crates/rift-http-proxy/Dockerfile.prebuilt" "installs curl" \
    "sed -i.bak 's/ca-certificates \&\& \\\\/ca-certificates curl \&\& \\\\/' crates/rift-http-proxy/Dockerfile.prebuilt"
  plant "HEALTHCHECK back to curl" "crates/rift-http-proxy/Dockerfile.prebuilt" "HEALTHCHECK still shells out to curl" \
    "sed -i.bak 's|CMD \[\"/usr/local/bin/rift\", \"healthcheck\"\]|CMD curl -fsS http://localhost:2525/ \|\| exit 1|' crates/rift-http-proxy/Dockerfile.prebuilt"
  plant "FROM unpinned" "crates/rift-http-proxy/Dockerfile.prebuilt" "unpinned base" \
    "sed -i.bak 's|^FROM debian:bookworm-slim@sha256:[a-f0-9]*|FROM debian:bookworm-slim|' crates/rift-http-proxy/Dockerfile.prebuilt"
  plant "CA bundle dropped" "crates/rift-http-proxy/Dockerfile.prebuilt" "has no CA bundle" \
    "sed -i.bak '/ca-certificates/d' crates/rift-http-proxy/Dockerfile.prebuilt"
  # Multi-stage: drop it from the RUNTIME stage only, leaving the builder's mention in place. A
  # file-wide grep would be satisfied by the builder and miss this entirely.
  plant "CA bundle dropped from runtime stage only" "crates/rift-http-proxy/Dockerfile" "has no CA bundle" \
    "awk '/^FROM debian/ { rt = 1 } { if (rt && /ca-certificates/) next; print }' crates/rift-http-proxy/Dockerfile > t && mv t crates/rift-http-proxy/Dockerfile"
  plant "static image installs packages" "crates/rift-http-proxy/Dockerfile.static" "runtime stage installs OS packages" \
    "sed -i.bak 's|^FROM scratch|FROM scratch\nRUN apt-get install -y make|' crates/rift-http-proxy/Dockerfile.static"
  plant "compose probes with curl" "docs/demo/docker-compose.yml" "healthchecks with curl" \
    "sed -i.bak 's|test: \[\"CMD\", \"rift\", \"healthcheck\"\]|test: [\"CMD\", \"curl\", \"-f\", \"http://localhost:2525/\"]|' docs/demo/docker-compose.yml"
  plant "provenance disabled" ".github/workflows/docker-publish.yml" "still sets provenance: false" \
    "sed -i.bak 's|provenance: mode=max|provenance: false|' .github/workflows/docker-publish.yml"
  # One unattested build step among several attested ones — the copy-paste regression.
  plant "one build step drops its SBOM" ".github/workflows/docker-publish.yml" "request an SBOM" \
    "perl -0pi -e 's/          sbom: true\n//' .github/workflows/docker-publish.yml"
  plant "one build step drops max provenance" ".github/workflows/docker-publish.yml" "request max provenance" \
    "perl -0pi -e 's/          provenance: mode=max\n//' .github/workflows/docker-publish.yml"
  plant "a publishing job drops cosign" ".github/workflows/docker-publish.yml" "install cosign" \
    "perl -0pi -e 's/        uses: sigstore\/cosign-installer\@v3\n//' .github/workflows/docker-publish.yml"
  plant "a publishing job drops id-token" ".github/workflows/docker-publish.yml" "id-token: write" \
    "perl -0pi -e 's/      id-token: write\n//' .github/workflows/docker-publish.yml"
  plant "dependabot file removed" ".github/dependabot.yml" "dependabot.yml is missing" \
    "rm -f .github/dependabot.yml"
  # The file exists but stops watching docker — the branch `rm` never reaches.
  plant "dependabot stops watching docker" ".github/dependabot.yml" "does not cover the docker ecosystem" \
    "sed -i.bak 's|package-ecosystem: \"docker\"|package-ecosystem: \"npm\"|' .github/dependabot.yml"

  echo
  if [ "$broken" -gt 0 ]; then
    echo "self-test FAILED: $broken plant(s) mutated nothing — fix the pattern, not the check" >&2
    exit 1
  fi
  if [ "$caught" -ne "$planted" ]; then
    echo "self-test FAILED: $caught/$planted violations caught" >&2
    exit 1
  fi
  echo "self-test passed: $caught/$planted planted violations caught"
  exit 0
}

case "${1:-}" in
  --self-test) self_test ;;
  "") ;;
  *) echo "usage: $0 [--self-test]" >&2; exit 64 ;;
esac

echo "verify-image-hardening: checking $ROOT"
if run_checks; then
  echo
  echo "image hardening OK"
  exit 0
else
  echo
  echo "image hardening FAILED: $FAILURES check(s) failed" >&2
  exit 1
fi
