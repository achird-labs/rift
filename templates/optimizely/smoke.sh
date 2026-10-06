#!/usr/bin/env bash
# Black-box smoke test for the Optimizely template. Needs a running Rift started with
#   rift --configfile imposters.json
# and curl + python3 on PATH. Exits non-zero on the first failed check.
#
# Every check is an explicit `if … then ok else fail`: a `cmd | grep -q x && echo ok` chain can
# report success for a check that never ran.
#
# Env overrides: CDN, LOGX, ODP, ADMIN, PROXY (base URLs; defaults are the template's ports).
set -euo pipefail

CDN=${CDN:-http://localhost:4600}
LOGX=${LOGX:-http://localhost:4601}
ODP=${ODP:-http://localhost:4602}
ADMIN=${ADMIN:-http://localhost:2525}
PROXY=${PROXY:-http://127.0.0.1:4610}
LM='Sat, 03 Oct 2026 12:00:00 GMT'
ODP_KEY='mock-odp-public-key'

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

ok()   { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; exit 1; }

# status <curl args...>: print the HTTP status only ("000" when the request never completed).
status() { curl -sS -m 10 -o /dev/null -w '%{http_code}' "$@" 2>/dev/null || true; }

# expect_status <label> <want> <curl args...>
expect_status() {
  local label="$1" want="$2" got
  shift 2
  got="$(status "$@")"
  if [ "$got" = "$want" ]; then ok "$label"; else fail "$label (want $want, got $got)"; fi
}

# expect_json <label> <python assertion over d> <curl args...>: the body must parse as JSON and
# satisfy the assertion. curl -f is deliberately not used: some checks assert on a 4xx body.
expect_json() {
  local label="$1" check="$2"
  shift 2
  if ! curl -sS -m 10 -o "$TMP/body" "$@"; then fail "$label (request failed)"; fi
  if python3 - "$TMP/body" "$check" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    d = json.load(f)
assert eval("(" + sys.argv[2] + ")"), d
PY
  then ok "$label"; else fail "$label"; fi
}

json_post=(-X POST -H 'Content-Type: application/json')

echo "cdn"
expect_status "GET datafile -> 200" 200 "$CDN/datafiles/MOCK_SDK_KEY.json"
expect_json "datafile body parses, 3 flags, ODP host points at the mock" \
  'd["version"] == "4" and d["revision"] == "1"
   and {f["key"] for f in d["featureFlags"]} == {"checkout_redesign", "vip_support", "legacy_search"}
   and d["integrations"][0]["host"].endswith(":4602")' \
  "$CDN/datafiles/MOCK_SDK_KEY.json"
curl -sS -m 10 -D "$TMP/headers" -o /dev/null "$CDN/datafiles/MOCK_SDK_KEY.json" || fail "GET datafile headers"
if tr -d '\r' <"$TMP/headers" | grep -qi "^last-modified: $LM\$"; then
  ok "Last-Modified header present"
else
  fail "Last-Modified header missing or wrong"
fi
expect_status "If-Modified-Since (current) -> 304" 304 -H "If-Modified-Since: $LM" "$CDN/datafiles/MOCK_SDK_KEY.json"
expect_status "If-Modified-Since (stale) -> 200" 200 -H "If-Modified-Since: Mon, 01 Jan 2024 00:00:00 GMT" "$CDN/datafiles/MOCK_SDK_KEY.json"
expect_status "authenticated datafile path -> 200" 200 -H 'Authorization: Bearer x' "$CDN/datafiles/auth/MOCK_SDK_KEY.json"
expect_status "unknown path -> 404" 404 "$CDN/nope"

echo "logx"
BATCH='{"account_id":"10000000001","project_id":"10000000002","revision":"1","client_name":"smoke","client_version":"0","anonymize_ip":true,"enrich_decisions":true,"visitors":[{"visitor_id":"user-1","attributes":[],"snapshots":[{"decisions":[],"events":[{"entity_id":"30000000001","type":"checkout_completed","key":"checkout_completed","timestamp":1,"uuid":"u1"}]}]}]}'
expect_status "POST /v1/events -> 204" 204 "${json_post[@]}" -d "$BATCH" "$LOGX/v1/events"
expect_status "POST /v1/events without visitor_id -> 400" 400 "${json_post[@]}" -d '{"visitors":[]}' "$LOGX/v1/events"

echo "odp"
# shellcheck disable=SC2016 # $userId is a GraphQL variable, not a shell one
Q='{"query":"query($userId: String, $audiences: [String]) {customer(fs_user_id: $userId) {audiences(subset: $audiences) {edges {node {name state}}}}}","variables":{"userId":"user-vip","audiences":["high_value_customers"]}}'
expect_json "graphql user-vip -> qualified" \
  'd["data"]["customer"]["audiences"]["edges"][0]["node"] == {"name": "high_value_customers", "state": "qualified"}' \
  "${json_post[@]}" -H "x-api-key: $ODP_KEY" -d "$Q" "$ODP/v3/graphql"
expect_json "graphql other user -> no segments" \
  'd["data"]["customer"]["audiences"]["edges"] == []' \
  "${json_post[@]}" -H "x-api-key: $ODP_KEY" -d "${Q/user-vip/user-42}" "$ODP/v3/graphql"
expect_json "graphql user-unknown -> InvalidIdentifierException" \
  'd["errors"][0]["extensions"]["classification"] == "InvalidIdentifierException"' \
  "${json_post[@]}" -H "x-api-key: $ODP_KEY" -d "${Q/user-vip/user-unknown}" "$ODP/v3/graphql"
expect_status "wrong x-api-key -> 403" 403 "${json_post[@]}" -H 'x-api-key: wrong' -d "$Q" "$ODP/v3/graphql"
expect_status "POST /v3/events -> 200" 200 "${json_post[@]}" -H "x-api-key: $ODP_KEY" \
  -d '[{"type":"fullstack","action":"identified","identifiers":{"fs_user_id":"user-1"},"data":{}}]' "$ODP/v3/events"

echo "intercept"
if curl -sSf -m 10 -o "$TMP/ca.pem" "$ADMIN/intercept/ca.pem"; then
  ok "intercept CA exported from the admin API"
else
  fail "GET /intercept/ca.pem"
fi
expect_status "https://cdn.optimizely.com via the intercept proxy -> 200" 200 \
  --proxy "$PROXY" --cacert "$TMP/ca.pem" "https://cdn.optimizely.com/datafiles/MOCK_SDK_KEY.json"
expect_json "the CDN imposter recorded the dialed host (Host: cdn.optimizely.com)" \
  'any(r["headers"].get("Host") == "cdn.optimizely.com" for r in d)' \
  "$ADMIN/imposters/4600/savedRequests"
expect_status "https://logx.optimizely.com via the intercept proxy -> 204" 204 \
  --proxy "$PROXY" --cacert "$TMP/ca.pem" "${json_post[@]}" -d "$BATCH" "https://logx.optimizely.com/v1/events"

echo "admin"
expect_json "recorded /v1/events requests readable via the admin API" \
  'len(d) >= 3' \
  "$ADMIN/imposters/4601/savedRequests?match=path=/v1/events"
expect_json "server-side verify: a segment lookup for user-vip happened" \
  'd["matched"] >= 1' \
  "${json_post[@]}" -d '{"predicates":[{"equals":{"path":"/v3/graphql"}},{"contains":{"body":"user-vip"}}]}' \
  "$ADMIN/imposters/4602/verify"
echo "all checks passed"
