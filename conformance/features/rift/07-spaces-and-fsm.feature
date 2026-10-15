@rift @spaces
Feature: Spaces (flow-id partitioning) and FSM declarative-scenario state

  Scenario: requests partition by space via the flow-id header, with a global fallback
    Given a fresh imposter from fixture "spaces/spaces"
    When the request header "X-Mock-Space" is "alice"
    And the client sends "GET" "/data"
    Then the response status is 200
    And the response body is "owner=alice"
    When the request header "X-Mock-Space" is "bob"
    And the client sends "GET" "/data"
    Then the response status is 200
    And the response body is "owner=bob"
    When the client sends "GET" "/health"
    Then the response status is 200
    And the response body is "OK"

  Scenario: spaces CRUD — register a stub into a new space, then serve and count it
    Given a fresh imposter from fixture "spaces/spaces"
    When the client registers a stub in space "gamma" for path "/data" returning "owner=gamma"
    Then space "gamma" has 1 stubs
    When the request header "X-Mock-Space" is "gamma"
    And the client sends "GET" "/data"
    Then the response status is 200
    And the response body is "owner=gamma"

  # A space-scoped stub outranks a global stub on the same path; an unknown space falls back to the
  # global stub. Both `/shared` stubs (space alice + global) match the same path.
  Scenario: a space-scoped stub takes precedence over a global stub on the same path
    Given a fresh imposter from fixture "spaces/spaces"
    When the request header "X-Mock-Space" is "alice"
    And the client sends "GET" "/shared"
    Then the response status is 200
    And the response body is "alice-shared"
    When the request header "X-Mock-Space" is "zzz"
    And the client sends "GET" "/shared"
    Then the response status is 200
    And the response body is "global-shared"

  # Teardown isolation: torn-down space's stubs are emptied AND it stops serving its old body (the
  # engine's no-match default: HTTP 200, empty body — not the old body, not a 404) — while a sibling
  # space and the global stub are unaffected. The pre-teardown check proves alpha was actually live,
  # so "stops serving" can't pass vacuously against a no-op registration.
  Scenario: tearing down a space empties its stubs and stops serving, without affecting siblings
    Given a fresh imposter from fixture "spaces/spaces"
    When the client registers a stub in space "alpha" for path "/data" returning "owner=alpha"
    And the client registers a stub in space "beta" for path "/data" returning "owner=beta"
    And the request header "X-Mock-Space" is "alpha"
    And the client sends "GET" "/data"
    Then the response body is "owner=alpha"
    When the client tears down space "alpha"
    Then space "alpha" has 0 stubs
    When the request header "X-Mock-Space" is "alpha"
    And the client sends "GET" "/data"
    Then the response status is 200
    And the response body is ""
    When the request header "X-Mock-Space" is "beta"
    And the client sends "GET" "/data"
    Then the response status is 200
    And the response body is "owner=beta"
    When the client sends "GET" "/health"
    Then the response status is 200
    And the response body is "OK"

  Scenario: a scenario stub advances only the flow-id it is addressed with (no cross-space bleed)
    Given a fresh imposter from fixture "spaces/spaces"
    When the request header "X-Mock-Space" is "alice"
    And the client sends "GET" "/step"
    Then the response status is 200
    And the response body is "stepped"
    And scenario "step" in space "alice" is in state "done"
    And scenario "step" in space "bob" is untouched

  Scenario: FSM sequential transitions from Started through to the terminal state
    Given a fresh imposter from fixture "spaces/fsm"
    When the client sends "GET" "/s"
    Then the response status is 200
    And the response body is "a"
    When the client sends "GET" "/s"
    Then the response status is 200
    And the response body is "b"
    And scenario "cas" is in state "two"
    When the client sends "GET" "/s"
    Then the response status is 200
    And the response body is "terminal"
    And scenario "cas" is in state "two"

  # rift#311: after a reset, a concurrent burst is inherently racy about *which* responses come back
  # ("a" vs "b"), so we deliberately do NOT assert the first-state hit count. The single deterministic
  # invariant we CAN assert is the end state: once a 20-way burst has fully drained (the burst step
  # joins every request), the scenario must have settled at the terminal state — no crash, no wedge,
  # no stuck-at-intermediate. (This proves settle-to-terminal, not per-transition monotonicity.)
  Scenario: FSM concurrent settle (rift#311) — a burst always settles to the terminal state
    Given a fresh imposter from fixture "spaces/fsm"
    When the client sends "GET" "/s"
    And the client sends "GET" "/s"
    And the client resets scenarios
    Then scenario "cas" is in state "Started"
    When the client fires 20 concurrent requests to "/s"
    Then all concurrent responses have status 200
    And scenario "cas" is in state "two"

  # rift 0.13.0 (rift#514): a stub whose scenarioName carries no requiredScenarioState/newScenarioState
  # now provisions a REAL flow store (was a silent no-op). The bare "bare" scenario therefore exists at
  # "Started", a direct PUT to its state persists (read back), and reset clears it back to "Started".
  Scenario: a bare scenarioName provisions a real, writable flow store
    Given a fresh imposter from fixture "spaces/bare-scenario"
    Then scenario "bare" is in state "Started"
    When the client sets scenario "bare" state to "custom"
    Then scenario "bare" is in state "custom"
    When the client resets scenarios
    Then scenario "bare" is in state "Started"
