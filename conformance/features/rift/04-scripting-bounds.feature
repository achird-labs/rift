@rift @scripting
Feature: Scripting engines and runaway bounds

  # Stateful: the ctx.state counter persists across requests to the same imposter, so this is one
  # ordered scenario (a fresh imposter resets the counter to 0).
  Scenario: rhai ctx.state counter increments across requests
    Given a fresh imposter from fixture "scripting/scripts"
    When the client sends "GET" "/rhai"
    Then the response status is 200
    And the response body is "1"
    When the client sends "GET" "/rhai"
    Then the response status is 200
    And the response body is "2"

  Scenario Outline: a <engine> script injects its response
    Given a fresh imposter from fixture "scripting/scripts"
    When the client sends "GET" "<path>"
    Then the response status is 200
    And the response body is "<body>"

    Examples:
      | engine | path | body  |
      | js     | /js  | js-ok |

  # rift#308: a runaway script is bounded by timeoutMs (300) — rather than hanging it returns in about
  # 300ms. rift 0.13.x refined the timeout contract: the status is now 504 Gateway Timeout (was 500 in
  # 0.12.0), carrying x-rift-script-error: true plus the dedicated x-rift-script-timeout: true header
  # that distinguishes a timeout from any other script error.
  Scenario: a runaway rhai script is bounded by its timeout
    Given a fresh imposter from fixture "scripting/runaway"
    When the client sends "GET" "/hang"
    Then the response status is 504
    And the response header "x-rift-script-error" is "true"
    And the response header "x-rift-script-timeout" is "true"
    And the response took at least 250 ms
    And the response took at most 2000 ms

  # scenarioParallelism=1 deliberately saturates the blocking-script threads. The burst is launched
  # in the background (fork), so the sibling + admin probes run WHILE the three runaways are still
  # in flight (the rift#308 property — the engine must not wedge during script execution). The burst
  # is joined last and every request must have been bounded (504 Gateway Timeout, per the 0.13.x
  # script-timeout contract).
  Scenario: a sibling imposter stays responsive while concurrent runaways are in flight
    Given a fresh verbatim imposter from fixture "scripting/sibling"
    And a fresh imposter from fixture "scripting/runaway"
    When the client fires 3 concurrent requests to "/hang"
    And the client sends "GET" "/alive" to port 4805
    Then the response status is 200
    And the response body is "alive"
    And the engine remains responsive
    And all concurrent responses have status 504

  # rift 0.13.x extends the 504 script-timeout contract (rift#499) beyond the `_rift.script` path
  # (covered above) to the mountebank inject sites. A slow response-inject times out with 504
  # "injection timeout"; the dedicated x-rift-script-timeout: true header marks it a timeout (vs any
  # other inject error), alongside x-rift-inject-error: true.
  Scenario: a slow response-inject times out with 504
    Given a fresh imposter from fixture "scripting/inject-response-timeout"
    When the client sends "GET" "/inject-response"
    Then the response status is 504
    And the response header "x-rift-script-timeout" is "true"
    And the response header "x-rift-inject-error" is "true"

  # A slow predicate-inject times out during matching with 504 "predicate injection timeout" —
  # same x-rift-script-timeout marker, distinguishing a timeout from an invalid-predicate error.
  Scenario: a slow predicate-inject times out with 504
    Given a fresh imposter from fixture "scripting/inject-predicate-timeout"
    When the client sends "GET" "/anything"
    Then the response status is 504
    And the response header "x-rift-script-timeout" is "true"
    And the response header "x-rift-inject-error" is "true"

  # A slow decorate behaviour times out AFTER the base response is chosen, so (non-strict) the
  # UNDECORATED response is still served — 200 with the original body — tagged x-rift-decorate-error
  # plus x-rift-script-timeout. Decorate never fails the response by default.
  Scenario: a slow decorate times out but still serves the undecorated response
    Given a fresh imposter from fixture "scripting/inject-decorate-timeout"
    When the client sends "GET" "/inject-decorate"
    Then the response status is 200
    And the response body is "DEC"
    And the response header "x-rift-script-timeout" is "true"
    And the response header "x-rift-decorate-error" is "true"

  # Under strictBehaviors, a decorate timeout instead fails loud — 504, still marked
  # x-rift-decorate-error + x-rift-script-timeout.
  Scenario: a slow decorate under strictBehaviors fails the response with 504
    Given a fresh imposter from fixture "scripting/inject-decorate-timeout-strict"
    When the client sends "GET" "/inject-decorate"
    Then the response status is 504
    And the response header "x-rift-script-timeout" is "true"
    And the response header "x-rift-decorate-error" is "true"

  # rift 0.13.0 (rift#500) made the non-timeout script *error* contracts stable and distinct from the
  # timeout ones above: a throwing (not slow) script fails fast, tagged x-rift-inject-error /
  # x-rift-decorate-error but WITHOUT x-rift-script-timeout. A response-inject that throws returns
  # 400 with the mountebank "invalid injection" error body.
  Scenario: a broken response-inject fails with 400 invalid injection
    Given a fresh imposter from fixture "scripting/inject-response-error"
    When the client sends "GET" "/inject-response"
    Then the response status is 400
    And the response header "x-rift-inject-error" is "true"
    And the response has no header "x-rift-script-timeout"
    And the response body contains "invalid injection"

  # A predicate-inject that throws fails matching with 400 "invalid predicate injection" — same
  # x-rift-inject-error marker, distinguishing an error from the 504 predicate-injection timeout.
  Scenario: a broken predicate-inject fails with 400 invalid predicate injection
    Given a fresh imposter from fixture "scripting/inject-predicate-error"
    When the client sends "GET" "/anything"
    Then the response status is 400
    And the response header "x-rift-inject-error" is "true"
    And the response has no header "x-rift-script-timeout"
    And the response body contains "invalid predicate injection"

  # A decorate that throws (non-strict) never fails the response: the UNDECORATED base response is
  # still served — 200 with the original body — tagged x-rift-decorate-error but not x-rift-script-timeout.
  Scenario: a broken decorate serves the undecorated 200 response
    Given a fresh imposter from fixture "scripting/inject-decorate-error"
    When the client sends "GET" "/inject-decorate"
    Then the response status is 200
    And the response body is "DEC"
    And the response header "x-rift-decorate-error" is "true"
    And the response has no header "x-rift-script-timeout"

  # Under strictBehaviors a throwing decorate instead fails loud — 500 Internal Server Error, still
  # marked x-rift-decorate-error (and still no x-rift-script-timeout: this is an error, not a timeout).
  Scenario: a broken decorate under strictBehaviors fails with 500
    Given a fresh imposter from fixture "scripting/inject-decorate-error-strict"
    When the client sends "GET" "/inject-decorate"
    Then the response status is 500
    And the response header "x-rift-decorate-error" is "true"
    And the response has no header "x-rift-script-timeout"
    And the response body contains "strictBehaviors"

  # Script API v2 (rift#442) result constructors beyond http(): a respond(ctx) can return delay(N)
  # (latency, then a default 200), reset() (a transport-level connection reset), or pass() (the
  # script ran but declined to inject, so a default empty 200 is served — still tagged x-rift-script).
  Scenario: a respond() returning delay(N) adds latency then serves a 200
    Given a fresh imposter from fixture "scripting/script-constructors"
    When the client sends "GET" "/delay"
    Then the response status is 200
    And the response took at least 250 ms
    And the response took at most 2000 ms

  Scenario: a respond() returning reset() aborts the connection at the transport level
    Given a fresh imposter from fixture "scripting/script-constructors"
    When the client sends "GET" "/reset" expecting a transport error
    Then the request fails at the transport level

  Scenario: a respond() returning pass() serves a default 200 without injecting
    Given a fresh imposter from fixture "scripting/script-constructors"
    When the client sends "GET" "/pass"
    Then the response status is 200
    And the response body is ""
    And the response header "x-rift-script" is "rhai"

  # ctx.state atomics beyond incr_by/get_or/delete (rift#443): exists() reflects a key's presence,
  # cas() does a compare-and-swap (applied only when the current value matches the expected), and a
  # `flowState.ttlSeconds` key expires in real time. One ordered scenario — the state persists across
  # requests to the same imposter (scenarioParallelism=1), so the wait observes a real expiry.
  Scenario: ctx.state cas/ttl/exists atomics
    Given a fresh imposter from fixture "scripting/state-atomics"
    When the client sends "GET" "/exists"
    Then the response body is "false"
    When the client sends "GET" "/set"
    Then the response body is "ok"
    When the client sends "GET" "/exists"
    Then the response body is "true"
    When the client sends "GET" "/cas"
    Then the response body is "true"
    When the client sends "GET" "/cas-stale"
    Then the response body is "false"
    When the client waits 3000 ms
    And the client sends "GET" "/ttl-get"
    Then the response body is "EXPIRED"
    When the client sends "GET" "/exists"
    Then the response body is "false"

  # ── the Script API ctx / result surface (rift docs/features/scripting.md, #195) ───────────────

  # A map body is JSON-serialised with Content-Type: application/json; `.header()` chains, and every
  # script response names its engine.
  Scenario: http() with a map body serialises JSON and chains a header
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the client sends "GET" "/http-map"
    Then the response status is 201
    And the response body is "{\"a\":1}"
    And the response header "content-type" is "application/json"
    And the response header "X-S" is "v"
    And the response header "x-rift-script" is "rhai"

  Scenario: delay(ms) answers 200 with an x-rift-latency-ms header
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the client sends "GET" "/delay"
    Then the response status is 200
    And the response header "x-rift-latency-ms" is a number at least 50

  Scenario: ctx.flowId is the flow resolved from flowIdSource
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the request header "X-Flow" is "flow-7"
    And the client sends "GET" "/flow-id"
    Then the response body is "flow-7"

  Scenario: ctx.stub exposes the stub's id, scenario name and scenario state
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the client sends "GET" "/stub"
    Then the response body is "ctx-stub|checkout|Started"

  Scenario: ctx.store.flow(id) writes another flow's state
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the request header "X-Flow" is "me"
    And the client sends "GET" "/cross-get"
    Then the response body is "none"
    When the client sends "GET" "/cross-set"
    And the client sends "GET" "/cross-get"
    Then the response body is "none"
    When the request header "X-Flow" is "other"
    And the client sends "GET" "/cross-get"
    Then the response body is "99"

  Scenario Outline: ctx.request.json parses a JSON body and is unit otherwise (<case>)
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the request body is "<body>"
    And the client sends "POST" "/json"
    Then the response body is "<result>"

    Examples:
      | case     | body      | result |
      | JSON     | {\"a\":7} | 7      |
      | not JSON | not json  | unit   |

  Scenario: ctx.request.header(name) is case-insensitive
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the request header "X-Mixed" is "yes"
    And the client sends "GET" "/header"
    Then the response body is "yes"

  Scenario: a javascript _rift.script respond(ctx) serves and names its engine
    Given a fresh imposter from fixture "scripting/ctx-surface"
    When the client sends "GET" "/js"
    Then the response body is "js"
    And the response header "x-rift-script" is "javascript"

  # rift#1159 (fixed in 0.18.0): an engine-less script under `defaultEngine: javascript` used to be
  # compiled as rhai. The JS-only syntax (arrow function, template literal) proves which engine ran,
  # and the resolved engine is written back into the imposter.
  Scenario: scriptEngine.defaultEngine javascript runs an engine-less script as JS and writes it back
    Given a fresh imposter from fixture "scripting/default-engine-js"
    When the client sends "GET" "/default-engine"
    Then the response status is 200
    And the response body is "js4"
    And the response header "x-rift-script" is "javascript"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs.0.responses.0._rift.script.engine" is "\"javascript\""

  Scenario: the removed lua engine is refused naming its replacement
    When the client posts the imposter fixture "scripting/engine-lua"
    Then the response status is 400
    And the error envelope message contains "Lua scripting engine was removed"
    And the error envelope message contains "use engine \"rhai\" or \"javascript\""
