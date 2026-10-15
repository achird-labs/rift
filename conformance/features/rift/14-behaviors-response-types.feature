@rift @behaviors
Feature: Behaviors and repeat on inject, proxy and fault responses (rift#1184)

  # rift#1192/#1194 (0.18.0): `_behaviors` now apply to inject and proxy responses, and a block
  # `repeat` cycles every response type. Before, the block was ignored on those types. Feature 06
  # stays scoped to `is` responses.

  # ── inject (rift#1192) ────────────────────────────────────────────────────────────────────────

  Scenario: decorate rewrites what an inject function returned
    Given a fresh imposter from fixture "behaviors-response-types/inject"
    When the client sends "GET" "/decorate"
    Then the response status is 200
    And the response body is "fn+dec"
    And the response header "X-Decorated" is "yes"

  Scenario: wait delays an inject response
    Given a fresh imposter from fixture "behaviors-response-types/inject"
    When the client sends "GET" "/wait"
    Then the response status is 200
    And the response body is "fn"
    And the response took at least 500 ms

  # A function that throws gets the existing injection-error response, and no behavior runs on it.
  Scenario: an inject function that throws runs no behavior
    Given a fresh imposter from fixture "behaviors-response-types/inject"
    When the client sends "GET" "/throws"
    Then the response status is 400
    And the response header "x-rift-inject-error" is "true"
    And the response has no header "X-Decorated"

  # ── proxy (rift#1194): the upstream is another imposter in the same engine ───────────────────

  Scenario: decorate rewrites a live proxied response
    Given an upstream imposter from fixture "behaviors-response-types/upstream"
    And a fresh imposter from fixture "behaviors-response-types/proxy" proxying to the upstream
    When the client sends "GET" "/always"
    Then the response status is 200
    And the response body is "upX"

  # The replay of the recorded stub is decorated once, not re-decorated on top of the stored body.
  Scenario: under proxyOnce the replay is decorated exactly once
    Given an upstream imposter from fixture "behaviors-response-types/upstream"
    And a fresh imposter from fixture "behaviors-response-types/proxy" proxying to the upstream
    When the client sends "GET" "/once"
    Then the response body is "upX"
    When the client sends "GET" "/once"
    Then the response body is "upX"
    And the upstream imposter has recorded 1 requests

  # The recorded stub holds the decorated body X(upstream) plus only the addDecorateBehavior script
  # Y, so its replay is Y(X(upstream)).
  Scenario: with predicateGenerators the recorded replay is Y(X(upstream))
    Given an upstream imposter from fixture "behaviors-response-types/upstream"
    And a fresh imposter from fixture "behaviors-response-types/proxy" proxying to the upstream
    When the client sends "GET" "/generated"
    Then the response body is "upX"
    When the client sends "GET" "/generated"
    Then the response body is "upXY"
    And the upstream imposter has recorded 1 requests

  Scenario: a failed decorate records nothing, so the next request goes upstream again
    Given an upstream imposter from fixture "behaviors-response-types/upstream"
    And a fresh imposter from fixture "behaviors-response-types/proxy" proxying to the upstream
    When the client sends "GET" "/failing"
    Then the response header "x-rift-decorate-error" is "true"
    When the client sends "GET" "/failing"
    Then the response header "x-rift-decorate-error" is "true"
    And the upstream imposter has recorded 2 requests

  # ── repeat on every response type ───────────────────────────────────────────────────────────

  Scenario: a block repeat cycles a proxy response
    Given an upstream imposter from fixture "behaviors-response-types/upstream"
    And a fresh imposter from fixture "behaviors-response-types/proxy" proxying to the upstream
    When the client sends "GET" "/repeat"
    Then the response body is "up"
    When the client sends "GET" "/repeat"
    Then the response body is "up"
    When the client sends "GET" "/repeat"
    Then the response body is "after"

  Scenario: a block repeat cycles an inject response
    Given a fresh imposter from fixture "behaviors-response-types/inject"
    When the client sends "GET" "/repeat"
    Then the response body is "fn"
    When the client sends "GET" "/repeat"
    Then the response body is "fn"
    When the client sends "GET" "/repeat"
    Then the response body is "after"

  # POST, not GET: the JDK client silently retries an idempotent GET once after a connection reset,
  # which would consume two of the repeat's slots per step.
  Scenario: a block repeat cycles a fault response
    Given a fresh imposter from fixture "behaviors-response-types/repeat"
    When the client sends "POST" "/fault" expecting a transport error
    Then the request fails at the transport level
    When the client sends "POST" "/fault" expecting a transport error
    Then the request fails at the transport level
    When the client sends "POST" "/fault"
    Then the response body is "after"

  Scenario: a top-level repeat wins over a block repeat
    Given a fresh imposter from fixture "behaviors-response-types/repeat"
    When the client sends "GET" "/top-level"
    Then the response body is "a"
    When the client sends "GET" "/top-level"
    Then the response body is "a"
    When the client sends "GET" "/top-level"
    Then the response body is "a"
    When the client sends "GET" "/top-level"
    Then the response body is "b"

  # ── rift-lint ───────────────────────────────────────────────────────────────────────────────

  Scenario: rift-lint refuses a top-level repeat that does not fit a u32
    Then rift-lint reports "E035" for fixture "behaviors-response-types/lint-repeat-overflow"

  Scenario: rift-lint raises no W017 for behaviors on inject and proxy blocks or a repeat-only block
    Then rift-lint reports no "W017" for fixture "behaviors-response-types/lint-clean"
