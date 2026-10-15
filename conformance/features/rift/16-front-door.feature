@rift
Feature: Front door routing and single-port gateway dispatch (rift docs/features/front-door.md, gateway.md)

  # The routing scenarios swap the shared engine for one started with `--front-door` and a
  # `routes` table (rift-fixtures/front-door/*.json: imposter 4872 answers "A", 4873 answers "B"),
  # the same swap feature 11 uses. @front-door is external-only: only the spawned binary takes the
  # flag. Requests carry an explicit Host header to the front-door port. "the front door answered
  # X" means 200 with body X (the imposter that served it), or, for "no-route", 404 carrying
  # `x-rift-front-door: no-route`.

  @front-door
  Scenario Outline: a wildcard host matches a real subdomain only, case-insensitively, ignoring :port
    Given the engine is started with the front door and routes fixture "front-door/host"
    When the client sends "GET" "/x" through the front door with host "<host>"
    Then the front door answered "<outcome>"

    Examples:
      | host              | outcome  |
      | api.pay.test      | A        |
      | API.PAY.TEST:8080 | A        |
      | pay.test          | no-route |
      | evilpay.test      | no-route |

  @front-door
  Scenario Outline: path_prefix matches on segment boundaries
    Given the engine is started with the front door and routes fixture "front-door/path-prefix"
    When the client sends "GET" "<path>" through the front door with host "any.test"
    Then the front door answered "<outcome>"

    Examples:
      | path          | outcome  |
      | /api/v1       | A        |
      | /api/v1/users | A        |
      | /api/v1x      | no-route |

  @front-door
  Scenario Outline: a headers clause matches the name case-insensitively and the first value byte-exactly
    Given the engine is started with the front door and routes fixture "front-door/headers"
    When the client sends "GET" "/x" through the front door with host "any.test" and headers "<headers>"
    Then the front door answered "<outcome>"

    Examples:
      | headers        | outcome  |
      | x-t: b         | A        |
      | x-t: b; x-t: a | A        |
      | X-T: B         | no-route |
      | x-t: a; x-t: b | no-route |

  @front-door
  Scenario Outline: a method clause is compared case-insensitively
    Given the engine is started with the front door and routes fixture "front-door/method"
    When the client sends "<method>" "/x" through the front door with host "any.test"
    Then the front door answered "<outcome>"

    Examples:
      | method | outcome  |
      | POST   | A        |
      | GET    | no-route |

  # Each table is authored so that neither authoring order nor id order picks the expected winner.
  @front-door
  Scenario Outline: evaluation order is priority, then specificity, then lowest id
    Given the engine is started with the front door and routes fixture "front-door/<table>"
    When the client sends "GET" "/x" through the front door with host "<host>" and headers "x-one: 1; x-two: 2"
    Then the front door answered "<outcome>"

    Examples:
      | table             | host          | outcome |
      | order-specificity | api.shop.test | A       |
      | order-priority    | api.shop.test | B       |
      | order-id          | any.test      | A       |

  @front-door
  Scenario: a disabled route is skipped even at a higher priority
    Given the engine is started with the front door and routes fixture "front-door/disabled"
    When the client sends "GET" "/x" through the front door with host "flag.test"
    Then the front door answered "B"

  @front-door
  Scenario: strip_prefix removes the matched prefix and keeps the query string
    Given the engine is started with the front door and routes fixture "front-door/targets"
    When the client sends "GET" "/svc/users?q=1" through the front door with host "any.test"
    Then the front door answered "A"
    And imposter 4872 last recorded path "/users" with query "q" equal to "1"

  @front-door
  Scenario: set_host rewrites the Host the imposter records
    Given the engine is started with the front door and routes fixture "front-door/targets"
    When the client sends "GET" "/x" through the front door with host "edge.test"
    Then the front door answered "A"
    And imposter 4872 last recorded header "Host" equal to "svc.internal"

  @front-door
  Scenario: a matched route whose imposter is missing is a 404 without the no-route marker
    Given the engine is started with the front door and routes fixture "front-door/targets"
    When the client sends "GET" "/x" through the front door with host "ghost.test"
    Then the response status is 404
    And the response has no header "x-rift-front-door"

  @front-door
  Scenario: an unmatched request falls back to the gateway's /__rift/{port} addressing
    Given the engine is started with the front door and routes fixture "front-door/host"
    When the client sends "GET" "/__rift/4873/x" through the front door with host "other.test"
    Then the front door answered "B"

  # rift#1234 (fixed in 0.19.0; #229): a tcp fault reached through the front door aborts the
  # connection exactly as the imposter's own port does (up to 0.18.1: an HTTP 502, empty body).
  # front-door/faults.json routes host fault.test to imposter 4872, which holds one stub per kind.
  @front-door
  Scenario Outline: a <kind> fault through the front door gives a <outcome> (rift#1234)
    Given the engine is started with the front door and routes fixture "front-door/faults"
    When the client sends a raw "GET" "<path>" through the front door with host "fault.test"
    Then the raw connection outcome is "<outcome>"

    Examples:
      | kind                     | path          | outcome                      |
      | CONNECTION_RESET_BY_PEER | /reset        | connection reset             |
      | top-level fault          | /topfault     | connection reset             |
      | script reset()           | /script-reset | connection reset             |
      | tcp empty                | /empty        | empty reply                  |
      | tcp garbage              | /garbage      | garbage bytes                |
      | MALFORMED_RESPONSE_CHUNK | /malformed    | 200 head then a broken chunk |

  # Over HTTP/2 (h2c) the fault resets only its own stream; the next stream on the same connection
  # is served.
  @front-door
  Scenario: a tcp fault through the front door over HTTP/2 resets only its stream (rift#1234)
    Given the engine is started with the front door and routes fixture "front-door/faults"
    When an h2c client sends "/reset" then "/ok" on one connection through the front door with host "fault.test"
    Then the first HTTP/2 stream was reset with INTERNAL_ERROR
    And the second HTTP/2 stream answered 200 "ok"

  @front-door
  Scenario Outline: an invalid route table fails the boot, naming the route and the rule
    When an engine is booted with the front door and routes fixture "front-door/<table>"
    Then the engine refused to boot with output naming "<route>" and "<rule>"

    Examples:
      | table                        | route                | rule                            |
      | invalid-duplicate-id         | route id 'twin'      | duplicate route id              |
      | invalid-identical-match      | 'first' and 'second' | match exactly the same requests |
      | invalid-strip-without-prefix | route 'nostrip'      | has no path_prefix to strip     |
      | invalid-wildcard             | route 'midwild'      | a wildcard is one leading '*.'  |
      | invalid-path-prefix          | route 'noslash'      | must start with '/'             |
      | invalid-method               | route 'badverb'      | is not a valid HTTP method      |

  # Gateway (untagged): served by the shared engine's admin port.
  Scenario: /__rift/{port} with no trailing path dispatches to /
    Given a fresh imposter from fixture "front-door/gateway"
    When the client sends "GET" "" via the gateway
    Then the response status is 200
    And the response body is "root"

  Scenario Outline: the gateway passes the query string through to the imposter's predicates
    Given a fresh imposter from fixture "front-door/gateway"
    When the client sends "GET" "<path>" via the gateway
    Then the response status is <status>
    And the response body is "<body>"

    Examples:
      | path        | status | body   |
      | /search?q=1 | 200    | q is 1 |
      | /search?q=2 | 404    | miss   |

  Scenario: a non-numeric gateway port is a 400 bad data
    When the client sends "GET" "/__rift/abc/x" to the admin port
    Then the response status is 400
    And the error envelope type is "bad data"

  Scenario: a gateway port with no imposter is a 404 no such resource
    When the client sends "GET" "/x" via the gateway to a port with no imposter
    Then the response status is 404
    And the error envelope type is "no such resource"
