@rift @admin
Feature: Admin surface — stub-by-id, enable/disable, savedRequests filtering, flow-state KV, atomic PUT /imposters

  Scenario: a stub is retrievable by its explicit id; an unknown id 404s
    Given a fresh imposter from fixture "admin/base"
    Then the stub with id "stub-hi" is retrievable
    And the stub with id "stub-hi" includes "/hi"
    And no stub has id "nope"

  Scenario: disabling an imposter 503s the data plane; enabling it restores 200
    Given a fresh imposter from fixture "admin/base"
    When the client disables the imposter
    And the client sends "GET" "/hi"
    Then the response status is 503
    When the client enables the imposter
    And the client sends "GET" "/hi"
    Then the response status is 200
    And the response body is "hi"

  Scenario: X-Rift-Debug returns diagnostics instead of the normal stub response
    Given a fresh imposter from fixture "admin/base"
    When the request header "X-Rift-Debug" is "true"
    And the client sends "GET" "/hi"
    Then the response status is 200
    And the response header "x-rift-debug-response" is "true"
    And the response body contains "/hi"

  Scenario: savedRequests ?match= filters by header, and stacked match= clauses AND together
    Given a fresh imposter from fixture "admin/base"
    When the request header "X-Tag" is "a"
    And the request header "X-Env" is "prod"
    And the client sends "GET" "/hi"
    When the request header "X-Tag" is "a"
    And the request header "X-Env" is "dev"
    And the client sends "GET" "/hi"
    When the request header "X-Tag" is "b"
    And the request header "X-Env" is "prod"
    And the client sends "GET" "/hi"
    Then recorded requests total 3
    And recorded requests matching header "X-Tag" value "a" total 2
    And recorded requests matching header "X-Tag" value "a" and header "X-Env" value "prod" total 1

  Scenario: a targeted savedRequests delete removes only the matching requests
    Given a fresh imposter from fixture "admin/base"
    When the request header "X-Tag" is "a"
    And the client sends "GET" "/hi"
    When the request header "X-Tag" is "b"
    And the client sends "GET" "/hi"
    Then recorded requests total 2
    When the client deletes recorded requests matching header "X-Tag" value "a"
    Then recorded requests total 1
    And recorded requests matching header "X-Tag" value "b" total 1

  # rift#494 / PR#512: server-side verify — POST /imposters/{port}/verify has the ENGINE evaluate a
  # predicate set against its own recorded requests, returning {matched, total}. Distinct from the
  # client-side savedRequests ?match= count above: the predicates are the full matcher language, and
  # includeClosest returns the best-scoring non-match with a per-clause failedPredicates diff.
  Scenario: server-side verify counts recorded requests matching a predicate set
    Given a fresh imposter from fixture "admin/base"
    When the client sends "GET" "/hi"
    And the client sends "GET" "/hi"
    And the client sends "GET" "/other"
    Then a verify for path "/hi" matches 2 of 3 recorded requests
    And a verify for path "/absent" matches 0 of 3 recorded requests

  Scenario: verify includeClosest reports the best non-match with a failedPredicates diff
    Given a fresh imposter from fixture "admin/base"
    When the client sends "GET" "/other"
    Then a verify for path "/absent" with includeClosest reports the closest request path "/other"

  # #94: verify's `flowId` scopes BOTH counts to one flow, resolved through the imposter's
  # `flowState.flowIdSource` (here the X-Space header) — the same scoping savedRequests
  # ?match=flow_id= applies. Without `flowId` the count spans every flow; an unseen flow is 0 of 0.
  Scenario: verify flowId scopes matched and total to one flow
    Given a fresh imposter from fixture "admin/verify-flows"
    When the request header "X-Space" is "blue"
    And the client sends "GET" "/hi"
    And the client sends "GET" "/hi"
    And the client sends "GET" "/other"
    And the request header "X-Space" is "green"
    And the client sends "GET" "/hi"
    Then a verify for path "/hi" matches 3 of 4 recorded requests
    And a verify for path "/hi" in flow "blue" matches 2 of 3 recorded requests
    And a verify for path "/hi" in flow "green" matches 1 of 1 recorded requests
    And a verify for path "/hi" in flow "absent" matches 0 of 0 recorded requests

  Scenario: flow-state KV full lifecycle — set, get, delete, then 404
    Given a fresh imposter from fixture "admin/base"
    When the client sets flow-state "k1" in flow "f1" to "v1"
    Then flow-state "k1" in flow "f1" is "v1"
    And flow-state "k1" in flow "f2" is absent
    When the client deletes flow-state "k1" in flow "f1"
    Then flow-state "k1" in flow "f1" is absent

  # @connect-refused: asserts a raw TCP connect is refused; a Docker-published port always accepts (container excludes it).
  @connect-refused
  Scenario: a valid PUT /imposters atomically swaps the whole imposter set
    Given a fresh verbatim imposter from fixture "admin/pre-existing"
    When the client sends "GET" "/old"
    Then the response status is 200
    And the response body is "old"
    When the client replaces all imposters with fixture "admin/replace-batch"
    Then the imposter on port 4808 no longer serves
    When the client sends "GET" "/swapped" to port 4809
    Then the response status is 200
    And the response body is "swapped"

  # @connect-refused: asserts a raw TCP connect is refused; a Docker-published port always accepts (container excludes it).
  @connect-refused
  Scenario: an invalid PUT /imposters batch is rejected and the previous set is untouched
    Given a fresh verbatim imposter from fixture "admin/pre-existing"
    When the client sends "GET" "/old"
    Then the response status is 200
    And the response body is "old"
    When the client replaces all imposters with an invalid batch
    # validate-then-swap: the rejected batch's port must never have been bound (no partial apply)…
    Then the imposter on port 4810 no longer serves
    # …and the previous set is untouched.
    When the client sends "GET" "/old"
    Then the response status is 200
    And the response body is "old"

  # rift#1219 (fixed in 0.18.1): `GET /imposters/{port}?replayable=true` serves the replayable
  # config — the same document `GET /imposters?replayable=true` lists — built from the LIVE stubs,
  # so a stub added after creation is exported too. Before 0.18.1 the route ignored `replayable`
  # and answered the detail view. Without the flag the detail view is unchanged; asserting both
  # makes the field checks discriminating. Runs on every leg: the embedded natives carry 0.18.1
  # since rift-java 0.3.3.
  Scenario: GET /imposters/{port}?replayable=true exports the live stubs without the detail fields
    Given a fresh imposter from fixture "admin/base"
    When the client sends admin "POST" "/imposters/{port}/stubs" with payload "admin/added-stub"
    Then the response status is 200
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response status is 200
    And the response JSON has top-level field "numberOfRequests"
    And the response JSON has top-level field "_links"
    When the client sends admin "GET" "/imposters/{port}?replayable=true" with payload "-"
    Then the response status is 200
    And the response JSON has no top-level field "requests"
    And the response JSON has no top-level field "numberOfRequests"
    And the response JSON has no top-level field "_links"
    And the response JSON has 2 stubs and none carries "_links"
    And the response body contains "/added"

  # ── journal filters + recorded-entry shape (#201, docs/api/index.md § savedRequests) ────────────
  # Three requests: GET /a (space s1), GET /b?x=1 (s1), POST /a with a body (s2) — journal order, so
  # the array positions below are deterministic. The body is staged last because a staged body
  # persists across sends. Every `match=` clause is AND-ed (#700).
  Scenario: savedRequests match= filters by method (case-sensitive), bare path and flow_id
    Given a fresh imposter from fixture "admin/journal-flows"
    When the request header "X-Space" is "s1"
    And the client sends "GET" "/a"
    And the client sends "GET" "/b?x=1"
    And the request header "X-Space" is "s2"
    And the request body is "hello"
    And the client sends "POST" "/a"
    When the client sends admin "GET" "/imposters/{port}/savedRequests?match=method=GET" with payload "-"
    Then the response is a JSON array of 2 elements
    When the client sends admin "GET" "/imposters/{port}/savedRequests?match=method=get" with payload "-"
    Then the response is a JSON array of 0 elements
    When the client sends admin "GET" "/imposters/{port}/savedRequests?match=path=/b" with payload "-"
    Then the response is a JSON array of 1 elements
    When the client sends admin "GET" "/imposters/{port}/savedRequests?match=flow_id=s1" with payload "-"
    Then the response is a JSON array of 2 elements
    When the client sends admin "GET" "/imposters/{port}/savedRequests?match=flow_id=s1&match=method=POST" with payload "-"
    Then the response is a JSON array of 0 elements

  # Journal indices start at 1 and `since` is exclusive, so after three requests the cursor is 3 —
  # passing it back verbatim yields nothing. A fresh journal reports 0.
  Scenario: savedRequests reports the x-rift-next-index cursor
    Given a fresh imposter from fixture "admin/journal-flows"
    When the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response header "x-rift-next-index" is "0"
    When the client sends "GET" "/a"
    And the client sends "GET" "/b"
    And the client sends "GET" "/c"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response is a JSON array of 3 elements
    And the response header "x-rift-next-index" is "3"
    When the client sends admin "GET" "/imposters/{port}/savedRequests?since=3" with payload "-"
    Then the response is a JSON array of 0 elements

  # 0.18.0 (#364/#940): status and latencyMs are recorded together; `body` only when there was one.
  Scenario: a recorded entry carries status and latencyMs, and body only when the request had one
    Given a fresh imposter from fixture "admin/journal-flows"
    When the client sends "GET" "/a"
    And the request body is "hello"
    And the client sends "POST" "/a"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response is a JSON array of 2 elements
    And every element of the response JSON array has a non-negative integer "status"
    And every element of the response JSON array has a non-negative integer "latencyMs"
    And the response JSON at "0.status" is the number 200
    And the response JSON at "0.body" is absent
    And the response JSON at "1.body" is the string "hello"
    And the response JSON at "0.requestFrom" is a non-empty string

  Scenario: a repeated header is recorded as a list in send order; a binary body as base64
    Given a fresh imposter from fixture "admin/journal-flows"
    When the client sends "GET" "/m" with header "X-M" repeated as "a" then "b"
    And the client posts the bytes "ff fe" to "/bin"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response JSON at "0.headers.X-M" has 2 elements
    And the response JSON at "0.headers.X-M.0" is the string "a"
    And the response JSON at "0.headers.X-M.1" is the string "b"
    And the response JSON at "1._mode" is the string "binary"
    And the response JSON at "1.body" is the string "//4="

  # docs/configuration/cli.md: an omitted, `null` or `0` port is assigned the first free port at or
  # above 49152, and the response carries it (#199). A batch creates its declared ports first.
  # @host-port: the engine auto-assigns a port outside the container's published pool (container excludes it).
  @host-port
  Scenario: POST /imposters without a port assigns one at or above 49152 and serves on it
    When the client creates a port-less imposter from fixture "admin/auto-port"
    Then the assigned port is at least 49152
    When the client sends "GET" "/auto"
    Then the response status is 200
    And the response body is "auto"

  # @host-port: the engine auto-assigns a port outside the container's published pool (container excludes it).
  @host-port
  Scenario: PUT /imposters keeps an explicit port and auto-assigns the port-less imposter
    When the client replaces all imposters with port-less batch fixture "admin/auto-port-batch"
    Then the assigned port is at least 49152
    When the client sends "GET" "/auto"
    Then the response body is "auto"
    When the client sends "GET" "/explicit" to port 4829
    Then the response status is 200
    And the response body is "explicit"
