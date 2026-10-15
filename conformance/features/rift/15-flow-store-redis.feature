@rift @redis
Feature: Flow-store backend parity — the in-memory and Redis stores give identical results

  # The backend-parity class of #115 (rift#537, #562, #987): the same flow-state scenario must behave
  # identically whether the imposter's `_rift.flowState.backend` is `inmemory` (the default) or
  # `redis`. Every outline runs its steps once per backend, against the SAME fixture — the step
  # forces the backend (and, for Redis, a `redis: {url, keyPrefix}` block per rift's
  # docs/features/flow-state.md) — and asserts literal values, so a divergence fails the Redis row
  # alone rather than both rows agreeing on a wrong answer.
  #
  # Docker-gated: the Redis store is a `redis:7-alpine` container the harness starts on first use and
  # stops at JVM exit (Ryuk is the backstop). Needs RIFT_IT=1 and the external engine, so the suite
  # sits outside `samples.rift.*` and runs only through the `engineRedis` alias.

  Scenario Outline: scenario-state transitions advance identically on the <backend> store
    Given a fresh "<backend>" flow-store imposter from fixture "flowstore/fsm"
    When the client sends "GET" "/step"
    Then the response status is 201
    And the response body is "started->paid"
    When the client sends "GET" "/step"
    Then the response status is 202
    And the response body is "paid->shipped"
    When the client sends "GET" "/step"
    Then the response status is 200
    And the response body is "shipped"
    # The terminal state has no newScenarioState, so the flow stays put.
    When the client sends "GET" "/step"
    Then the response body is "shipped"

    Examples:
      | backend  |
      | inmemory |
      | redis    |

  Scenario Outline: ctx.state incr_by / exists / cas / ttl behave identically on the <backend> store
    Given a fresh "<backend>" flow-store imposter from fixture "flowstore/atomics"
    When the client sends "GET" "/incr"
    Then the response body is "5"
    When the client sends "GET" "/incr"
    Then the response body is "10"
    When the client sends "GET" "/exists"
    Then the response body is "false"
    When the client sends "GET" "/set"
    And the client sends "GET" "/exists"
    Then the response body is "true"
    # cas answers "<applied>:<value after>" — a stale expected value must leave v2 in place.
    When the client sends "GET" "/cas"
    Then the response body is "true:v2"
    When the client sends "GET" "/cas-stale"
    Then the response body is "false:v2"
    # ttl(key, 0) deletes now and reports the key existed; ttl on an absent key reports false.
    When the client sends "GET" "/ttl-kill"
    Then the response body is "true:false"
    When the client sends "GET" "/exists"
    Then the response body is "false"
    When the client sends "GET" "/ttl-absent"
    Then the response body is "false"

    Examples:
      | backend  |
      | inmemory |
      | redis    |

  Scenario Outline: a key expires after the flow-state ttlSeconds on the <backend> store
    Given a fresh "<backend>" flow-store imposter from fixture "flowstore/expiry"
    When the client sends "GET" "/set"
    And the client sends "GET" "/get"
    Then the response body is "v1"
    When the client waits 2500 ms
    And the client sends "GET" "/get"
    Then the response body is "EXPIRED"

    Examples:
      | backend  |
      | inmemory |
      | redis    |

  # `{{ state.* }}` reads the value from BEFORE this request's own stateOps increment, so the first
  # response carries an empty header value and the count trails by one.
  Scenario Outline: {{ state.* }} renders into a response header identically on the <backend> store
    Given a fresh "<backend>" flow-store imposter from fixture "flowstore/header"
    When the client sends "GET" "/hit"
    Then the response header "X-Hits" is ""
    When the client sends "GET" "/hit"
    Then the response header "X-Hits" is "1"
    When the client sends "GET" "/hit"
    Then the response header "X-Hits" is "2"

    Examples:
      | backend  |
      | inmemory |
      | redis    |

  # Parity alone would also pass if the Redis imposter silently fell back to an in-memory store, so
  # this proves the state really lives in the container.
  Scenario: the Redis imposter's state is written to Redis under its own key prefix
    Given a fresh "redis" flow-store imposter from fixture "flowstore/atomics"
    When the client sends "GET" "/set"
    Then the Redis flow store holds key "k" for this imposter
