@rift @admin
Feature: Admin REST long tail — system routes, listing views, delete-all, teardown, stub surgery, spaces, scenarios, body cap

  # docs/api/index.md (v0.18.1). Every literal below was probed with curl against a standalone
  # 0.18.1 engine first (rift-conformance#200). Feature 08 owns stub-by-id GET, enable/disable,
  # debug, savedRequests and verify; feature 13 owns the 400/404 envelope sweep.

  # ── system routes ────────────────────────────────────────────────────────────────────────────
  Scenario: GET / links to imposters, config and logs
    When the client sends admin "GET" "/" with payload "-"
    Then the response status is 200
    And the response JSON at "_links.imposters.href" is a non-empty string
    And the response JSON at "_links.config.href" is a non-empty string
    And the response JSON at "_links.logs.href" is a non-empty string

  Scenario: GET /health reports ok
    When the client sends admin "GET" "/health" with payload "-"
    Then the response status is 200
    And the response JSON at "status" is the string "ok"

  Scenario: GET /logs is always empty and echoes its default window
    When the client sends admin "GET" "/logs" with payload "-"
    Then the response status is 200
    And the response JSON at "logs" is an empty array
    And the response JSON at "_links.self.href" is the string "/logs?startIndex=0&endIndex=100"

  # ── imposter listing views ───────────────────────────────────────────────────────────────────
  # #713: the list is in ascending port order regardless of creation order. The shared engine may
  # hold other scenarios' imposters, so the order and field checks look only at this scenario's two.
  Scenario: GET /imposters lists in ascending port order with stubCount and recordRequests
    Given imposters "hi" then "lo" from fixture "admin-rest/two-stubs" on descending ports
    When the client sends admin "GET" "/imposters" with payload "-"
    Then the response status is 200
    And the imposter list shows "lo" before "hi"
    And the listed imposters "lo,hi" all carry field "stubCount"
    And the listed imposters "lo,hi" all carry field "recordRequests"

  Scenario: GET /imposters?list=true returns short entries without stubCount
    Given imposters "hi" then "lo" from fixture "admin-rest/two-stubs" on descending ports
    When the client sends admin "GET" "/imposters?list=true" with payload "-"
    Then the response status is 200
    And the imposter list shows "lo" before "hi"
    And the listed imposters "lo,hi" all carry field "port"
    And the listed imposters "lo,hi" carry no field "stubCount"

  Scenario: removeProxies strips the proxy stub from the single and the all-imposters export
    Given a fresh imposter "px" from fixture "admin-rest/with-proxy"
    When the client sends admin "GET" "/imposters/{port}?replayable=true&removeProxies=true" with payload "-"
    Then the response status is 200
    And the response JSON at "stubs" has 1 elements
    And the response body does not contain "proxy"
    When the client sends admin "GET" "/imposters?replayable=true&removeProxies=true" with payload "-"
    Then the response status is 200
    And the listed imposter "px" has 1 stubs
    And the listed imposter "px" does not contain "proxy"

  # ── teardown ─────────────────────────────────────────────────────────────────────────────────
  # #596/#597: the delete returns only after the listener is unbound, so a connect issued right
  # after the 200 is refused — no retry, no settle window.
  # @connect-refused: asserts a raw TCP connect is refused; a Docker-published port always accepts (container excludes it).
  @connect-refused
  Scenario: DELETE /imposters/{port} returns a zero-count snapshot after the listener is gone
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends "GET" "/a"
    Then a TCP connect to the current imposter is accepted
    When the client sends admin "DELETE" "/imposters/{port}" with payload "-"
    Then the response status is 200
    And the response JSON at "numberOfRequests" is the number 0
    And the response JSON at "requests" is an empty array
    And an immediate TCP connect to the current imposter is refused

  # ── stub surgery ─────────────────────────────────────────────────────────────────────────────
  Scenario: POST stubs with an index inserts there; a duplicate id is a 409 conflict
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "POST" "/imposters/{port}/stubs" with payload "admin-rest/stub-mid-at-1"
    Then the response status is 200
    When the client sends admin "GET" "/imposters/{port}/stubs" with payload "-"
    Then the response JSON at "stubs" has 3 elements
    And the response JSON at "stubs.0.id" is the string "a"
    And the response JSON at "stubs.1.id" is the string "mid"
    And the response JSON at "stubs.2.id" is the string "b"
    And the response JSON at "stubs.1._links.self.href" is a non-empty string
    When the client sends admin "POST" "/imposters/{port}/stubs" with payload "admin-rest/stub-mid-at-1"
    Then the response status is 409
    And the response is a rift error envelope
    And the error envelope type is "resource conflict"
    And the error envelope message contains "mid"

  Scenario: PUT by-id replaces a stub in place; DELETE by-id removes it
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "POST" "/imposters/{port}/stubs" with payload "admin-rest/stub-mid-at-1"
    And the client sends admin "PUT" "/imposters/{port}/stubs/by-id/mid" with payload "admin-rest/stub-mid-v2"
    Then the response status is 200
    When the client sends admin "GET" "/imposters/{port}/stubs" with payload "-"
    Then the response JSON at "stubs" has 3 elements
    And the response JSON at "stubs.1.id" is the string "mid"
    And the response JSON at "stubs.1.responses.0.is.body" is the string "mid2"
    When the client sends "GET" "/mid"
    Then the response body is "mid2"
    When the client sends admin "DELETE" "/imposters/{port}/stubs/by-id/mid" with payload "-"
    Then the response status is 200
    When the client sends admin "GET" "/imposters/{port}/stubs/by-id/mid" with payload "-"
    Then the response status is 404
    And the error envelope type is "no such resource"

  # ── spaces ───────────────────────────────────────────────────────────────────────────────────
  Scenario: a space takes a bare stub and reports it on both space GETs
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "POST" "/imposters/{port}/spaces/s1/stubs" with payload "admin-rest/space-bare-stub"
    Then the response status is 201
    And the response JSON at "space" is the string "s1"
    And the response JSON at "stubs" has 1 elements
    When the client sends admin "GET" "/imposters/{port}/spaces/s1/stubs" with payload "-"
    Then the response status is 200
    And the response JSON at "space" is the string "s1"
    And the response JSON at "stubs" has 1 elements
    When the client sends admin "GET" "/imposters/{port}/spaces/s1" with payload "-"
    Then the response status is 200
    And the response JSON has top-level field "space"
    And the response JSON has top-level field "stubs"
    And the response JSON has top-level field "scenarios"
    And the response JSON has top-level field "numberOfRequests"

  # #932: a body that is not a stub is refused rather than becoming a match-everything stub.
  Scenario: a space refuses an enveloped or non-stub body but takes an empty-predicates default
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "POST" "/imposters/{port}/spaces/s1/stubs" with payload "admin-rest/space-envelope"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "envelope"
    When the client sends admin "POST" "/imposters/{port}/spaces/s1/stubs" with payload "admin-rest/not-a-stub"
    Then the response status is 400
    And the error envelope type is "bad data"
    When the client sends admin "POST" "/imposters/{port}/spaces/s1/stubs" with payload "admin-rest/empty-predicates"
    Then the response status is 201
    And the response JSON at "space" is the string "s1"

  # rift#1230 (docs fixed in 0.19.0): a space POST refuses `{}` with 400 "body is not a stub" — it
  # is the no-predicate, no-response stub #932 keeps out; docs/api/index.md now says to name a field
  # (`{"predicates": [], ...}`, the 201 above). The engine was already right; this pins it.
  Scenario: a space refuses an empty-object body (rift#1230)
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "POST" "/imposters/{port}/spaces/s1/stubs" with payload "admin-rest/empty-object"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "not a stub"

  # ── scenarios ────────────────────────────────────────────────────────────────────────────────
  Scenario: GET scenarios echoes flowId; PUT state without a state is a 400
    Given a fresh imposter from fixture "admin-rest/two-stubs"
    When the client sends admin "GET" "/imposters/{port}/scenarios?flowId=alice" with payload "-"
    Then the response status is 200
    And the response JSON at "flowId" is the string "alice"
    And the response JSON at "scenarios" is an empty array
    When the client sends admin "PUT" "/imposters/{port}/scenarios/x/state" with payload "admin-rest/empty-object"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "state"

  # ── request-body cap (#567) ──────────────────────────────────────────────────────────────────
  # The admin API caps bodies at 64 MiB: one byte over is a 413 before the JSON is parsed; exactly
  # 64 MiB is read and rejected as bad JSON instead — the boundary is inclusive.
  Scenario: an admin body one byte over 64 MiB is a 413; exactly 64 MiB is not
    When the client posts 67108865 bytes to admin "/imposters"
    Then the response status is 413
    And the response is a rift error envelope
    And the error envelope type is "request too large"
    When the client posts 67108864 bytes to admin "/imposters"
    Then the response status is 400
    And the error envelope type is "bad data"

  # ── delete-all (dedicated engine) ────────────────────────────────────────────────────────────
  # DELETE /imposters would wipe every other scenario's imposters on the shared engine, so these
  # run on a raw-spawned sidecar engine (external mode only; excluded from engineEmbedded).
  @admin-isolated
  Scenario: DELETE /imposters returns every imposter and leaves none
    Given an isolated engine with imposters "iso1" from "admin-rest/two-stubs" and "iso2" from "admin-rest/with-proxy"
    When the client sends isolated admin "DELETE" "/imposters" with payload "-"
    Then the response status is 200
    And the response lists exactly the imposters "iso1,iso2"
    When the client sends isolated admin "GET" "/imposters" with payload "-"
    Then the response status is 200
    And the response JSON at "imposters" is an empty array

  # rift#1229 (fixed in 0.19.0): the delete-all body is the document GET ?replayable=true lists just
  # before, so a stub added at runtime is in it (the Mountebank save-before-reset idiom). Up to
  # 0.18.1 it was each imposter as CREATED (2 stubs).
  @admin-isolated
  Scenario: the DELETE /imposters snapshot keeps a stub added at runtime (rift#1229)
    Given an isolated engine with imposters "iso1" from "admin-rest/two-stubs" and "iso2" from "admin-rest/with-proxy"
    When the client sends isolated admin "POST" "/imposters/{iso1}/stubs" with payload "admin-rest/stub-added"
    And the client sends isolated admin "GET" "/imposters?replayable=true" with payload "-"
    Then the listed imposter "iso1" has 3 stubs
    When the client sends isolated admin "DELETE" "/imposters" with payload "-"
    Then the response status is 200
    And the listed imposter "iso1" has 3 stubs
