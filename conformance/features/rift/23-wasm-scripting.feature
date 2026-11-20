@rift @scripting @wasm
Feature: WebAssembly components as _rift.script (engine "wasm")

  # rift#1751. The fixtures carry each component as base64 `code`, the form resolution writes from a
  # `.wasm` file and a saved imposter keeps. They are generated from hand-written core modules
  # (crates/rift-mock-core/test-support/wasm_fixtures.rs) and pinned by a drift test in
  # crates/rift-http-proxy/tests/wasm_scripting.rs.

  Scenario: a component answers with its status, headers and body
    Given a fresh imposter from fixture "scripting/wasm"
    When the client sends "GET" "/ok"
    Then the response status is 200
    And the response body is "ok"
    And the response header "x-rift-script" is "wasm"
    And the response header "x-twin" is "yes"

  # The component writes `count` through the rift:mock/state import; the Rhai script reads it.
  Scenario: a component and a Rhai script share the imposter's flow store
    Given a fresh imposter from fixture "scripting/wasm"
    When the client sends "GET" "/incr"
    Then the response status is 200
    When the client sends "GET" "/incr"
    Then the response status is 200
    When the client sends "GET" "/read"
    Then the response status is 200
    And the response body is "2"

  # Deny by implementation: wasi:sockets is never linked, so the component cannot be instantiated.
  Scenario: a component importing wasi:sockets is refused at admission
    When the client posts the imposter fixture "scripting/wasm-sockets"
    Then the response status is 400
    And the error envelope message contains "wasm component imports wasi:sockets/tcp@0.2.6, which Rift does not provide"
