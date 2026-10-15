@rift @flowstate
Feature: Declarative flow state — _rift.stateOps, fail-loud flowState config, the clear-flow admin route

  # rift 0.18.0 (#969) added `_rift.stateOps`: declarative writes on an `is` response, run in order
  # after the render and before the write (rift docs/features/flow-state.md). Feature 15 covers only
  # `increment` into a header, and only on the Docker `engineRedis` leg; this feature pins every op,
  # the pre-op template read, integer coercion, the skip-on-fault rule and auto-provisioning on the
  # default in-memory store, plus the fail-loud `flowState` config doors and the clear-flow admin
  # route (#530). Every fixture keys its flow on `X-Flow`, so a scenario isolates its flows by name.
  #
  # Values read back through `GET /admin/imposters/{p}/flow-state/{flow}/{key}` are asserted by JSON
  # TYPE as well as value: `set "42"` must store the number 42, `set "007"` the string "007".

  Background:
    Given a fresh imposter from fixture "stateops/ops"
    And the request header "X-Flow" is "f1"

  # `{{ state.n }}` renders the value from BEFORE this request's own increment.
  Scenario: increment creates the key at 0, and the template reads the pre-op value
    When the client sends "GET" "/inc"
    Then the response body is "n="
    When the client sends "GET" "/inc"
    Then the response body is "n=1"
    When the client sends "GET" "/inc"
    Then the response body is "n=2"
    And flow-state "n" in flow "f1" is the number 3

  Scenario: increment honours `by`
    When the client sends "GET" "/inc5"
    Then flow-state "m" in flow "f1" is the number 5

  Scenario: set renders previousValue — the key's value before this op
    When the client sends "GET" "/set"
    Then flow-state "s" in flow "f1" is "vx"
    When the client sends "GET" "/set"
    Then flow-state "s" in flow "f1" is "vvxx"

  Scenario: set stores a canonical integer string as a number and anything else as a string
    When the client sends "GET" "/coerce"
    Then flow-state "a" in flow "f1" is the number 42
    And flow-state "b" in flow "f1" is "007"
    And flow-state "c" in flow "f1" is "4.5"

  Scenario: delete removes the key
    When the client sends "GET" "/inc"
    Then flow-state "n" in flow "f1" is the number 1
    When the client sends "GET" "/del"
    Then flow-state "n" in flow "f1" is absent

  Scenario: clearFlow empties the requesting flow and leaves other flows alone
    When the client sends "GET" "/inc"
    And the client sends "GET" "/inc5"
    And the request header "X-Flow" is "f2"
    And the client sends "GET" "/inc"
    And the request header "X-Flow" is "f1"
    And the client sends "GET" "/clear"
    Then flow-state "n" in flow "f1" is absent
    And flow-state "m" in flow "f1" is absent
    And flow-state "n" in flow "f2" is the number 1

  Scenario: stateOps do not run when a fault pre-empts the response
    When the client sends "GET" "/fault"
    Then the response status is 503
    And the response header "x-rift-fault" is "error"
    And flow-state "f" in flow "f1" is absent

  Scenario: a wait delays stateOps but does not skip them
    When the client sends "GET" "/wait"
    Then the response status is 200
    And the response took at least 200 ms
    And flow-state "w" in flow "f1" is the number 1

  # Outside debug mode a failing template token renders as an empty string with a `rift::template`
  # warning (rift docs/features/date-templates.md "When a token fails"), so the `set` stores "" and
  # every op after it still runs; the response serves normally.
  Scenario: without --debug an unrenderable set value stores "" and the remaining ops still run
    When the client sends "GET" "/bad"
    Then the response status is 200
    And the response body is "ok"
    And the response has no header "x-rift-state-ops-error"
    And flow-state "pre" in flow "f1" is the number 1
    And flow-state "x" in flow "f1" is ""
    And flow-state "post" in flow "f1" is the number 1

  # Under `--debug` the first failing op aborts the rest with a 500; the ops before it already ran.
  # Needs an engine spawned with the flag, so it is external-only (excluded in the embedded and
  # container aliases).
  @rift-debug-env
  Scenario: under --debug the first failing op aborts the rest with a 500
    Given a fresh imposter from fixture "stateops/ops" on an engine started with "--debug"
    When the client sends "GET" "/bad"
    Then the response status is 500
    And the response header "x-rift-state-ops-error" is "true"
    And the error envelope message contains "unknown template function"
    And flow-state "pre" in flow "f1" is the number 1
    And flow-state "x" in flow "f1" is absent
    And flow-state "post" in flow "f1" is absent

  # No `_rift.flowState` at all: a stateOps stub gets an in-memory store at the default
  # `flowIdSource` (`imposter_port`), so the flow id is the imposter's port.
  Scenario: stateOps on an imposter with no flowState auto-provision an in-memory store
    Given a fresh imposter from fixture "stateops/auto"
    When the client sends "GET" "/inc"
    Then the response status is 200
    And flow-state "z" in flow "{port}" is the number 1

  Scenario Outline: flowState config is fail-loud — <case>
    When the client posts the imposter fixture "stateops/<fixture>"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "<message>"

    Examples:
      | case                                    | fixture                | message                                  |
      | ttlSeconds 0                            | config-ttl-zero        | flowState.ttlSeconds must be >= 1 (got 0) |
      | a negative ttlSeconds                   | config-ttl-negative    | flowState.ttlSeconds must be >= 1 (got -5) |
      | an unregistered backend                 | config-unknown-backend | no such backend is registered            |
      | redis backend without a redis block     | config-redis-no-block  | no redis config block was provided       |
      | redis backend with a flat url           | config-redis-flat-url  | no redis config block was provided       |
      | a rediss:// url                         | config-rediss-url      | Failed to parse Redis URL                |
      | a redis+sentinel:// url                 | config-sentinel-url    | Failed to parse Redis URL                |

  Scenario: DELETE /admin/imposters/{port}/flow-state/{flowId} clears the flow and is idempotent
    When the client sends "GET" "/inc"
    And the request header "X-Flow" is "f2"
    And the client sends "GET" "/inc"
    When the client sends admin "DELETE" "/admin/imposters/{port}/flow-state/f1" with payload "-"
    Then the response status is 200
    And the response JSON field "cleared" is "true"
    And the response JSON field "flowId" is "\"f1\""
    And flow-state "n" in flow "f1" is absent
    And flow-state "n" in flow "f2" is the number 1
    When the client sends admin "DELETE" "/admin/imposters/{port}/flow-state/f1" with payload "-"
    Then the response status is 200
    And the response JSON field "cleared" is "true"

  Scenario: clearing a flow on a port with no imposter is a 404
    When the client sends admin "DELETE" "/admin/imposters/{unknownPort}/flow-state/f1" with payload "-"
    Then the response status is 404
    And the error envelope type is "no such resource"

  Scenario: PUT of a flow-state key without a value is a 400
    When the client sends admin "PUT" "/admin/imposters/{port}/flow-state/f1/k" with payload "stateops/empty-object"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "missing required field: value"

  # rift#973 / #978: `_rift.sequencing` (imposter) and `_rift.dataset` (response) are carried for an
  # embedder / Rift Cluster and never executed by standalone Rift: they round-trip through the
  # replayable export unchanged, and the stub serves its `is` body as if they were absent.
  Scenario: the carried-not-executed sequencing and dataset blocks round-trip unchanged
    Given a fresh imposter from fixture "stateops/carriers"
    When the client sends "GET" "/ds"
    Then the response status is 200
    And the response body is "served"
    When the client sends admin "GET" "/imposters/{port}?replayable=true" with payload "-"
    Then the response status is 200
    And the response JSON at "_rift.sequencing" equals the same path in fixture "stateops/carriers"
    And the response JSON at "stubs.0.responses.0._rift.dataset" equals the same path in fixture "stateops/carriers"
