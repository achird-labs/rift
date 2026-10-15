@rift @transport
Feature: Single-port gateway, binary-mode responses, self-signed HTTPS, and Prometheus metrics

  Scenario: the single-port gateway routes to the addressed imposter
    Given a fresh imposter from fixture "gateway/services"
    When the client sends "GET" "/g" via the gateway
    Then the response status is 200
    And the response body is "gwbody"

  Scenario: a binary-mode response body is base64-decoded on the wire
    Given a fresh imposter from fixture "gateway/services"
    When the client sends "GET" "/bin"
    Then the response status is 200
    And the response body is "hello"

  Scenario: an HTTPS imposter auto-generates a self-signed cert and serves over TLS
    Given a fresh verbatim imposter from fixture "gateway/secure"
    When the client sends "GET" "/sec" over HTTPS to port 4812
    Then the response status is 200
    And the response body is "secure"
    # …and the cert really is self-signed: a client that verifies the chain must reject it.
    And a cert-verifying HTTPS request to port 4812 at "/sec" is rejected

  Scenario: the metrics endpoint counts data-plane requests
    Given a fresh imposter from fixture "gateway/services"
    When the client records the request counter
    And the client sends "GET" "/ping"
    And the client sends "GET" "/ping"
    And the client sends "GET" "/ping"
    Then the request counter increased by at least 3
    And the metrics endpoint reports at least 1 requests

  # rift#977 (0.18.0): `mutualAuth` REQUIRES a client certificate (Mountebank only requests one), and
  # `rejectUnauthorized` + `ca` validate it. Before, a mutualAuth imposter silently accepted every
  # client. Refusal happens in the TLS handshake, so no HTTP status ever arrives. Test certs and how
  # they were made: src/test/plane-a/rift-fixtures/tls/README.md.
  Scenario: a mutualAuth imposter refuses a client that presents no certificate
    Given a fresh imposter from fixture "tls/mutual-auth-ca"
    Then an HTTPS request to "/m" presenting no client certificate is refused

  Scenario: a mutualAuth imposter validating against the test CA accepts only a cert that CA signed
    Given a fresh imposter from fixture "tls/mutual-auth-ca"
    When the client sends "GET" "/m" over HTTPS presenting client certificate "client"
    Then the response status is 200
    And the response body is "mtls-ca"
    And an HTTPS request to "/m" presenting client certificate "other-client" is refused

  # Without `rejectUnauthorized` (rift refuses `ca` without it, and `rejectUnauthorized: false` with a
  # `ca`, as 400), any client certificate is accepted, but one is still required.
  Scenario: a mutualAuth imposter without rejectUnauthorized accepts a client cert from any issuer
    Given a fresh imposter from fixture "tls/mutual-auth-any"
    When the client sends "GET" "/m" over HTTPS presenting client certificate "other-client"
    Then the response status is 200
    And the response body is "mtls-any"
    And an HTTPS request to "/m" presenting no client certificate is refused

  # rift#974 (0.18.0): `--upstream-ca-file` appends a private CA to the outbound trust store. The
  # upstream is an HTTPS imposter serving a leaf signed by the committed test CA. The shared engine
  # (no flag) is the control: its proxy refuses that leaf (UnknownIssuer) with a 502. A dedicated
  # engine started with `--upstream-ca-file <test CA>` proxies it. External-only: no other mode can
  # spawn that second engine.
  @upstream-trust
  Scenario: a proxy trusts an HTTPS upstream only when its CA is given to --upstream-ca-file
    Given an HTTPS upstream imposter from fixture "tls/upstream"
    When a proxy imposter to the HTTPS upstream is created
    And the client sends "GET" "/up"
    Then the response status is 502
    And the response header "x-rift-proxy-error" is "true"
    When a proxy imposter to the HTTPS upstream is created on an engine trusting the test CA
    And the client sends "GET" "/up"
    Then the response status is 200
    And the response body is "upstream-tls-ok"
