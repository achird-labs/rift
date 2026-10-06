#!/bin/bash
# End-to-end check of the intercept demo (docker-compose-intercept.yml must be up):
#   1. the SUT container gets the datafile through rift's intercept listener;
#   2. rift serves the CA the SUT trusts;
#   3. editing the datafile on disk and reloading changes what the SUT gets — no admin call
#      touches the rules;
#   4. a rule added at runtime over the admin API serves a host the file does not route.
set -euo pipefail

cd "$(dirname "$0")"
COMPOSE=(docker compose -f docker-compose-intercept.yml)
ADMIN="${ADMIN:-http://localhost:2525}"
DATAFILE=fixtures/datafile.json
ORIGINAL="$(cat "$DATAFILE")"
trap 'printf "%s\n" "$ORIGINAL" > "$DATAFILE"; curl -fsS -X POST "$ADMIN/admin/reload" >/dev/null || true' EXIT

sut_get() {
  "${COMPOSE[@]}" exec -T sut curl -fsS --cacert /certs/rift-ca.pem "https://$1"
}

# Fail loudly: `cmd | grep -q x && echo ok` would skip a failed check without tripping `set -e`.
expect() {
  local what="$1" haystack="$2" needle="$3"
  if ! grep -q -- "$needle" <<<"$haystack"; then
    echo "   FAIL: $what (wanted '$needle', got: $haystack)" >&2
    exit 1
  fi
  echo "   ok: $what"
}

echo "1. The SUT fetches the datafile through the intercept proxy"
expect "revision 1" "$(sut_get cdn.optimizely.com/datafiles/demo.json)" '"revision": "1"'

echo "2. Rift serves the CA the SUT trusts"
if ! curl -fsS "$ADMIN/intercept/ca.pem" | cmp -s - intercept-ca/ca-cert.pem; then
  echo "   FAIL: rift's CA differs from intercept-ca/ca-cert.pem" >&2
  exit 1
fi
echo "   ok: same certificate"

echo "3. Edit the datafile and reload"
printf '%s\n' "$ORIGINAL" | sed 's/"revision": "1"/"revision": "2"/' > "$DATAFILE"
reload="$(curl -fsS -X POST "$ADMIN/admin/reload")"
expect "the reload re-seeded the file's intercept rule" "$reload" '"rulesSeeded"'
expect "the SUT now sees revision 2" "$(sut_get cdn.optimizely.com/datafiles/demo.json)" '"revision": "2"'

echo "4. A rule added at runtime"
curl -fsS -X POST "$ADMIN/intercept/rules" -d '{
  "host": "logx.optimizely.com",
  "action": { "serve": { "statusCode": 204 } }
}' >/dev/null
status="$("${COMPOSE[@]}" exec -T sut curl -sS -o /dev/null -w '%{http_code}' \
  --cacert /certs/rift-ca.pem -X POST https://logx.optimizely.com/v1/events)"
expect "an event POST is answered by the runtime rule" "$status" "204"

echo "All intercept demo checks passed."
