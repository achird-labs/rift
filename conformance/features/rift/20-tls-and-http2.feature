@rift @tls
Feature: TLS termination options and HTTP/2 negotiation (rift docs/features/tls.md, #977/#1029)

  # Certificates are the committed test material under rift-fixtures/tls/ (see its README): leaves
  # signed by the test CA in rift-fixtures/intercept/. Every HTTPS client here trusts ONLY that CA, so
  # a handshake that succeeds proves the imposter served the fixture chain, not a self-signed default.
  #
  # Untagged scenarios drive the admin API of the shared engine (external + embedded). @tls-flags
  # scenarios need an engine started with a TLS CLI flag or environment variable, so each spawns a
  # dedicated engine: external only, excluded from the embedded and container aliases.

  Scenario Outline: an HTTPS imposter serves an inline <kind> cert and key
    Given an HTTPS imposter serving "<cert>" with key "<key>"
    When an HTTPS GET trusting only the test CA is sent to the imposter
    Then the response status is 200
    And the peer certificate subject is "CN=rift-conformance-test-server-leaf"

    Examples:
      | kind   | cert            | key                 |
      | PKCS#8 | server-leaf.pem | server-leaf.key     |
      | PKCS#1 | server-leaf.pem | server-leaf-rsa.key |

  Scenario: an inline chain is served leaf first
    Given an HTTPS imposter serving the chain "chain-leaf.pem" then "intermediate.pem" with key "chain-leaf.key"
    When an HTTPS GET trusting only the test CA is sent to the imposter
    Then the response status is 200
    And the peer certificate chain has 2 certificates, the first with subject "CN=rift-conformance-test-chain-leaf"

  Scenario Outline: an invalid HTTPS or mutual-TLS imposter is refused 400 (<case>)
    When an imposter with "<case>" is posted
    Then the response status is 400
    And the response body contains "bad data"
    And the response body contains "TLS configuration error:"
    And the response body contains "<message>"

    Examples:
      | case                                  | message                                   |
      | a key that does not match the cert    | cert/key mismatch?                        |
      | a cert without a key                  | must provide both                         |
      | rejectUnauthorized without mutualAuth | require `mutualAuth: true`                |
      | ca without rejectUnauthorized         | `ca` requires `rejectUnauthorized: true`  |
      | rejectUnauthorized without ca         | `rejectUnauthorized` requires `ca`        |
      | an empty ca list                      | `rejectUnauthorized` requires `ca`        |
      | a ca with no certificate              | contains no certificate                   |
      | mutualAuth on an http imposter        | `mutualAuth` requires `protocol:          |

  Scenario: mutualAuth false on an http imposter is accepted
    When an imposter with "mutualAuth false on an http imposter" is posted
    Then the response status is 201

  # A client certificate must carry the clientAuth EKU; the test CA signing it is not enough.
  Scenario: a client certificate whose EKU lacks clientAuth is refused
    Given a fresh imposter from fixture "tls/mutual-auth-ca"
    When the client sends "GET" "/m" over HTTPS presenting client certificate "client"
    Then the response status is 200
    And an HTTPS request to "/m" presenting client certificate "client-serverauth-only" is refused

  # rift#1029: an imposter offers h2 over ALPN unless something on it needs HTTP/1 semantics — a TCP
  # fault or a `_rift.script` — in which case it offers only http/1.1.
  Scenario Outline: ALPN on an HTTPS imposter <with> negotiates <version> (rift#1029)
    Given an HTTPS imposter serving "server-leaf.pem" with key "server-leaf.key" and "<stubs>"
    When an HTTP/2-preferring HTTPS GET trusting only the test CA is sent to the imposter
    Then the response was served over "<version>"

    Examples:
      | with                   | stubs                    | version  |
      | a plain stub           | a plain stub             | HTTP_2   |
      | a _rift.fault.tcp stub | a _rift.fault.tcp stub   | HTTP_1_1 |
      | a _rift.script stub    | a _rift.script stub      | HTTP_1_1 |

  Scenario: a plain-HTTP imposter accepts h2c with prior knowledge
    Given a plain-HTTP imposter answering 200 "over-h2c"
    When an h2c prior-knowledge GET is sent to the imposter
    Then the h2c response has status 200 and body "over-h2c"

  @tls-flags
  Scenario: --default-tls-cert/--default-tls-key serve an HTTPS imposter that has no cert
    Given an engine started with the default TLS cert "server-leaf.pem" and key "server-leaf.key"
    And an HTTPS imposter with no cert on that engine
    When an HTTPS GET trusting only the test CA is sent to the imposter
    Then the response status is 200
    And the peer certificate subject is "CN=rift-conformance-test-server-leaf"

  @tls-flags
  Scenario: --no-self-signed-tls refuses an HTTPS imposter that has no cert
    Given an engine started with "--no-self-signed-tls"
    When an HTTPS imposter with no cert is posted to that engine
    Then the response status is 400
    And the response body contains "self-signed generation is disabled"

  @tls-flags
  Scenario: a missing --default-tls-cert file stops the engine at startup
    Then an engine started with a default TLS cert file that does not exist exits before serving

  @tls-flags
  Scenario: RIFT_DISABLE_HTTP2=1 makes an HTTPS imposter negotiate HTTP/1.1
    Given an engine started with the environment "RIFT_DISABLE_HTTP2" set to "1"
    And an HTTPS imposter serving "server-leaf.pem" with key "server-leaf.key" on that engine
    When an HTTP/2-preferring HTTPS GET trusting only the test CA is sent to the imposter
    Then the response was served over "HTTP_1_1"

  # The shared engine is the control: it does not trust the upstream's CA, so its proxy answers 502.
  @tls-flags
  Scenario: --upstream-tls-skip-verify proxies to an HTTPS upstream signed by an untrusted CA
    Given an HTTPS upstream imposter from fixture "tls/upstream"
    When a proxy imposter to the HTTPS upstream is created
    And the client sends "GET" "/up"
    Then the response status is 502
    And the response header "x-rift-proxy-error" is "true"
    When a proxy imposter to the HTTPS upstream is created on an engine started with "--upstream-tls-skip-verify"
    And the client sends "GET" "/up"
    Then the response status is 200
    And the response body is "upstream-tls-ok"
