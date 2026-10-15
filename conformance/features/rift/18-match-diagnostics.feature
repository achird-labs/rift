@rift @diagnostics
Feature: Match diagnostics — _rift.warnings stub analysis, the X-Rift-Debug report, journal matchOutcome

  # rift docs/features/stub-analysis.md, debug-mode.md and docs/api/index.md § Why a request did not
  # match (v0.18.1). Every literal below was probed with curl against a standalone 0.18.1 engine
  # first (rift-conformance#193). Feature 08 keeps its coarse X-Rift-Debug smoke test.

  # ── stub analysis: GET /imposters/{port} → _rift.warnings[] ─────────────────────────────────
  # `fields` are the exact values the warning carries: a number, a string, or `absent`.
  Scenario Outline: stub analysis flags <type> (<case>)
    Given a fresh imposter from fixture "diagnostics/<fixture>"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response status is 200
    And the response has a "<type>" warning with "<fields>" and a message containing "<message>"

    Examples:
      | case                                  | fixture              | type                 | fields                                    | message                     |
      | two stubs share an id                 | duplicate-id         | duplicate_id         | stubIndex=1,stubId=x,shadowedByIndex=0    | duplicate ID 'x'            |
      | identical predicates                  | exact-duplicate      | exact_duplicate      | stubIndex=1,shadowedByIndex=0             | will never match            |
      | startsWith /a before equals /a/b      | potentially-shadowed | potentially_shadowed | stubIndex=1,shadowedByIndex=0             | partially shadowed          |
      | a catch-all as the last stub          | catch-all-last       | catch_all            | stubIndex=1                               | will match ALL requests     |
      | a catch-all followed by one stub      | catch-all-not-last   | catch_all_not_last   | stubIndex=0                               | will shadow 1 stub          |
      | recordMatches at imposter level       | record-matches       | config_key_ignored   | stubIndex=absent                          | recordMatches               |
      | _rift on a proxy response             | rift-on-proxy        | config_key_ignored   | stubIndex=0                               | `_rift` on a `proxy`        |
      | stateOps on a script-only response    | state-ops-script     | state_ops_never_runs | stubIndex=0                               | never execute               |

  Scenario: an imposter with nothing to warn about and no flowState carries no _rift key
    Given a fresh imposter from fixture "diagnostics/clean"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response status is 200
    And the response JSON has no top-level field "_rift"

  # 102 identical stubs raise 101 exact_duplicate warnings: the first 100 are listed, then one
  # `truncated` entry counts the rest.
  Scenario: warnings are capped at 100 with a truncated entry
    Given a fresh imposter from fixture "diagnostics/many-duplicates"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "_rift.warnings" has 101 elements
    And the response JSON at "_rift.warnings.100.warningType" is the string "truncated"
    And the response JSON at "_rift.warnings.100.message" contains "suppressed (showing first 100)"

  Scenario: the analysis is recomputed after a stub is added and after it is deleted
    Given a fresh imposter from fixture "diagnostics/clean"
    When the client sends admin "POST" "/imposters/{port}/stubs" with payload "diagnostics/duplicate-stub"
    And the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response has a "exact_duplicate" warning with "stubIndex=1,stubId=d,shadowedByIndex=0" and a message containing "will never match"
    When the client sends admin "DELETE" "/imposters/{port}/stubs/by-id/d" with payload "-"
    And the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON has no top-level field "_rift"

  # ── X-Rift-Debug ──────────────────────────────────────────────────────────────────────────────
  Scenario: a matched debug request reports the stub and a response preview
    Given a fresh imposter from fixture "diagnostics/debug"
    When the request header "X-Rift-Debug" is "1"
    And the client sends "GET" "/c"
    Then the response status is 200
    And the response JSON at "debug" is the boolean true
    And the response JSON at "request.method" is the string "GET"
    And the response JSON at "request.path" is the string "/c"
    And the response JSON at "request.headers.X-Rift-Debug" is absent
    And the response JSON at "request.headers.x-rift-debug" is absent
    And the response JSON at "imposter.stubCount" is the number 2
    And the response JSON at "matchResult.matched" is the boolean true
    And the response JSON at "matchResult.stubIndex" is the number 1
    And the response JSON at "matchResult.stubId" is the string "cyc"
    And the response JSON at "matchResult.responsePreview.responseType" is the string "is"
    And the response JSON at "matchResult.responsePreview.statusCode" is the number 201
    And the response JSON at "matchResult.responsePreview.bodyPreview" is the string "A-body"

  # The header name is case-insensitive and `true` is accepted as well as `1`.
  Scenario: an unmatched debug request says why and lists every stub
    Given a fresh imposter from fixture "diagnostics/debug"
    When the request header "x-rift-debug" is "TRUE"
    And the client sends "GET" "/nowhere"
    Then the response status is 200
    And the response JSON at "matchResult.matched" is the boolean false
    And the response JSON at "matchResult.reason" is the string "No stub predicates matched the request"
    And the response JSON at "matchResult.allStubs" has 2 elements

  Scenario: a debug request on an imposter with no stubs says so
    Given a fresh imposter from fixture "diagnostics/empty"
    When the request header "X-Rift-Debug" is "true"
    And the client sends "GET" "/x"
    Then the response JSON at "matchResult.matched" is the boolean false
    And the response JSON at "matchResult.reason" is the string "No stubs configured for this imposter"
    And the response JSON at "matchResult.allStubs" is an empty array

  Scenario: X-Rift-Debug values other than true/1 take the normal path
    Given a fresh imposter from fixture "diagnostics/debug"
    When the request header "X-Rift-Debug" is "yes"
    And the client sends "GET" "/one"
    Then the response status is 200
    And the response body is "one"

  # The preview must not consume a response slot: after a debug request the cycle still starts at A.
  Scenario: the debug preview does not advance response cycling
    Given a fresh imposter from fixture "diagnostics/debug"
    When the request header "X-Rift-Debug" is "1"
    And the client sends "GET" "/c"
    And the request header "X-Rift-Debug" is removed
    And the client sends "GET" "/c"
    Then the response status is 201
    And the response body is "A-body"

  # ── journal matchOutcome (0.17.0, #909) ───────────────────────────────────────────────────────
  Scenario: a hit is journaled with matched, stubIndex and stubId
    Given a fresh imposter from fixture "diagnostics/match-outcome"
    When the client sends "GET" "/hit"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response is a JSON array of 1 elements
    And the response JSON at "0.matchOutcome.matched" is the boolean true
    And the response JSON at "0.matchOutcome.stubIndex" is the number 3
    And the response JSON at "0.matchOutcome.stubId" is the string "hit"

  # Visit order: stub 0 fails on its SECOND predicate (the header), stub 1 is gated by scenario
  # state, stub 2 by space. The /hit stub is ruled out by the candidate index before the scan, so
  # it is not listed at all.
  Scenario: a miss is journaled with every visited stub and why it fell out
    Given a fresh imposter from fixture "diagnostics/match-outcome"
    When the client sends "GET" "/m"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response JSON at "0.matchOutcome.matched" is the boolean false
    And the response JSON at "0.matchOutcome.tried" has 3 elements
    And the response JSON at "0.matchOutcome.tried.0.stubId" is the string "s0"
    And the response JSON at "0.matchOutcome.tried.0.why.reason" is the string "failedPredicate"
    And the response JSON at "0.matchOutcome.tried.0.why.predicateIndex" is the number 1
    And the response JSON at "0.matchOutcome.tried.1.stubId" is the string "s1"
    And the response JSON at "0.matchOutcome.tried.1.why.reason" is the string "skippedScenarioState"
    And the response JSON at "0.matchOutcome.tried.2.stubId" is the string "s2"
    And the response JSON at "0.matchOutcome.tried.2.why.reason" is the string "skippedSpace"

  # rift#1227 (fixed in 0.19.0): a journal entry records status/latencyMs only for an answer the
  # client received, so the debug entry leaves both out, as the doc says. Up to 0.18.1 it carried the
  # debug report's 200.
  Scenario: a debug request is journaled without a matchOutcome, status or latencyMs (rift#1227)
    Given a fresh imposter from fixture "diagnostics/match-outcome"
    When the request header "X-Rift-Debug" is "1"
    And the client sends "GET" "/hit"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response is a JSON array of 1 elements
    And the response JSON at "0.path" is the string "/hit"
    And the response JSON at "0.matchOutcome" is absent
    And the response JSON at "0.status" is absent
    And the response JSON at "0.latencyMs" is absent
