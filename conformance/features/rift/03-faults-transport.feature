@rift @transport
Feature: Faults and transport errors

  # Transport-level faults abort the connection — the client sees a failure, not an HTTP response.
  Scenario Outline: <kind> aborts the connection at the transport level
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "<path>" expecting a transport error
    Then the request fails at the transport level

    Examples:
      | kind                   | path   |
      | tcp reset              | /reset |
      | random-data-then-close | /rand  |

  Scenario: latency fault delays the response past its floor
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/slow"
    Then the response status is 200
    And the response took at least 600 ms

  Scenario: error fault sets the status, body and headers
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/err"
    Then the response status is 503
    And the response body contains "DOWN"
    And the response header "Retry-After" is "30"

  # Precedence tcp > error > latency (rift#277): the latency delay applies, then the tcp reset wins
  # so the error fault never surfaces — the client sees a transport failure after the delay.
  Scenario: fault precedence — tcp wins over error, after the latency delay
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/all" expecting a transport error
    Then the request fails at the transport level
    And the response took at least 600 ms

  Scenario: probability 0 disables a fault
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/p0"
    Then the response status is 200
    And the response body is "ok"

  # rift#309: a top-level `fault` response should abort the connection (Mountebank parity), not
  # return HTTP 502. This build (0.11.3) resets at the transport level, so the historical @gap is
  # resolved — we assert the correct behavior, which now guards against a regression.
  # https://github.com/achird-labs/rift/issues/309
  Scenario: top-level fault resets the connection at the transport level (rift#309)
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/topfault" expecting a transport error
    Then the request fails at the transport level

  # ── remaining fault injection (rift docs/features/fault-injection.md, #196) ─────────────────────

  # Every tcp kind has aliases (native.md § TCP Faults) that must behave as the canonical kind; a
  # malformed chunk is a decoding failure in the client, which surfaces as a transport error too.
  Scenario Outline: tcp <alias> fails the request at the HTTP client
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "<path>" expecting a transport error
    Then the request fails at the transport level

    Examples:
      | alias             | path             |
      | reset             | /reset-alias     |
      | garbage           | /garbage-alias   |
      | MALFORMED_RESPONSE_CHUNK | /malformed |
      | malformed         | /malformed-alias |

  # EMPTY_RESPONSE closes the connection without writing a single byte.
  Scenario Outline: tcp <kind> closes the connection with zero response bytes
    Given a fresh imposter from fixture "faults/faults"
    When the client sends a raw "GET" "<path>"
    Then the raw response is empty

    Examples:
      | kind           | path         |
      | EMPTY_RESPONSE | /empty       |
      | empty          | /empty-alias |

  # MALFORMED_RESPONSE_CHUNK writes a real status line and a chunked header, then a chunk size that
  # does not parse, then closes — the HTTP client above fails decoding the body.
  Scenario Outline: tcp <kind> writes a status line and then a malformed chunk
    Given a fresh imposter from fixture "faults/faults"
    When the client sends a raw "GET" "<path>"
    Then the raw response starts with "HTTP/1.1 200"
    And the raw response contains "Transfer-Encoding: chunked"

    Examples:
      | kind                     | path             |
      | MALFORMED_RESPONSE_CHUNK | /malformed       |
      | malformed                | /malformed-alias |

  # rift#533: the probabilistic object form `{probability, type}`; p=1 and p=0 bound it.
  Scenario: the tcp object form fires at probability 1 and never at probability 0
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/tcp-p1" expecting a transport error
    Then the request fails at the transport level
    When the client sends "GET" "/tcp-p0"
    Then the response status is 200
    And the response body is "ok"

  Scenario: the tcp string form reads back as a string and the object form as an object
    Given a fresh imposter from fixture "faults/faults"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response status is 200
    And the response JSON at "stubs.0.responses.0._rift.fault.tcp" is "\"CONNECTION_RESET_BY_PEER\""
    And the response JSON at "stubs.11.responses.0._rift.fault.tcp" is "\"reset\""
    And the response JSON at "stubs.13.responses.0._rift.fault.tcp" is "{\"probability\":1.0,\"type\":\"reset\"}"

  Scenario: a tcp object without a probability is refused
    When the client posts the imposter fixture "faults/tcp-object-no-probability"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "requires a numeric 'probability'"

  # A fixed `ms` overrides the `minMs`/`maxMs` range (both 0 here, so a range-only engine would not wait).
  Scenario: latency ms is a fixed delay that overrides minMs/maxMs
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/lat-ms"
    Then the response status is 200
    And the response took at least 300 ms

  Scenario: an error fault carries x-rift-fault
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/err"
    Then the response status is 503
    And the response header "x-rift-fault" is "error"

  # rift#1050: a single-valued header object that names one header twice (case-insensitively).
  Scenario: error fault headers naming one header twice are refused
    When the client posts the imposter fixture "faults/error-headers-dup"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "header `x-a` is already given as `X-A`"

  # Floors only, generous ceilings (the #126 lesson).
  Scenario: a wait range delays the response within the range
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/wait-range"
    Then the response status is 200
    And the response took at least 200 ms
    And the response took at most 2000 ms

  Scenario: an inverted wait range is refused naming both values
    When the client posts the imposter fixture "faults/wait-inverted"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "min 5 greater than max 1"

  Scenario: a wait {inject} function delays the response and reads back in the inject spelling
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/wait-inject"
    Then the response status is 200
    And the response took at least 150 ms
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs.17.responses.0.behaviors.0.wait" is "{\"inject\":\"function(){ return 150; }\"}"

  Scenario: a stub-level delayRange becomes a per-response wait
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/delay-range"
    Then the response status is 200
    And the response body is "dr"
    And the response took at least 200 ms

  # Response-type precedence is is > proxy > inject > fault: a top-level fault beside an `is` is dropped.
  Scenario: an is response beats a top-level fault on the same response
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/is-beats-fault"
    Then the response status is 200
    And the response body is "is-wins"

  Scenario: a top-level fault accepts the tcp aliases
    Given a fresh imposter from fixture "faults/faults"
    When the client sends "GET" "/topfault-alias" expecting a transport error
    Then the request fails at the transport level

  # A tcp fault on an HTTPS imposter is a genuine socket fault under TLS: the handshake completes,
  # then the connection is reset before any HTTP byte.
  Scenario: a tcp reset on an HTTPS imposter resets after the TLS handshake
    Given a fresh imposter from fixture "faults/https"
    When the client sends a raw "GET" "/reset" over TLS
    Then the TLS handshake completed
    And the raw response is empty

  # ── a tcp fault reached through the /__rift/ gateway (rift#1234, fixed in 0.19.0; #229) ─────────
  # The admin port's `/__rift/{port}` gateway gives exactly what the imposter's own port gives. Up
  # to 0.18.1 it framed every tcp fault as an HTTP 502 with an empty body.
  #
  # @tcp-rst: a published Docker port turns the RST into a clean EOF (it does so on the imposter's
  # own port too), so the container leg cannot tell a reset from an empty reply.
  @tcp-rst
  Scenario Outline: a <kind> fault through the gateway resets the connection (rift#1234)
    Given a fresh imposter from fixture "faults/doors"
    When the client sends a raw "GET" "<path>" via the gateway
    Then the raw connection outcome is "connection reset"

    Examples:
      | kind                     | path          |
      | CONNECTION_RESET_BY_PEER | /reset        |
      | top-level fault          | /topfault     |
      | script reset()           | /script-reset |

  Scenario Outline: a <kind> fault through the gateway gives a <outcome> (rift#1234)
    Given a fresh imposter from fixture "faults/doors"
    When the client sends a raw "GET" "<path>" via the gateway
    Then the raw connection outcome is "<outcome>"

    Examples:
      | kind                     | path          | outcome                      |
      | tcp empty                | /empty        | empty reply                  |
      | tcp garbage              | /garbage      | garbage bytes                |
      | MALFORMED_RESPONSE_CHUNK | /malformed    | 200 head then a broken chunk |

  # The gateway negotiates HTTP/2 (h2c), which an imposter with a tcp fault never does on its own
  # port: there the fault resets only that request's stream, and the connection keeps working.
  Scenario Outline: a <kind> fault through the gateway over HTTP/2 resets only its stream (rift#1234)
    Given a fresh imposter from fixture "faults/doors"
    When an h2c client sends "<path>" then "/ok" on one connection via the gateway
    Then the first HTTP/2 stream was reset with INTERNAL_ERROR
    And the second HTTP/2 stream answered 200 "ok"

    Examples:
      | kind                     | path          |
      | CONNECTION_RESET_BY_PEER | /reset        |
      | script reset()           | /script-reset |
