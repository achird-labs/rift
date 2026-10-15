@rift @behaviors
Feature: Response behaviors — copy, lookup, wait, shellTransform, decorate

  Scenario: copy from a request header into the response
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the request header "X-Request-Id" is "abc123"
    And the client sends "GET" "/ch"
    Then the response status is 200
    And the response body is "rid=abc123"

  Scenario: copy via jsonpath from the request body
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the request body is "{\"user\":{\"name\":\"alice\"}}"
    And the client sends "POST" "/cj"
    Then the response status is 200
    And the response body is "who=alice"

  # `lookup` reads data/products.csv relative to the engine's cwd, which RiftHarness sets to
  # $RIFT_CORPUS_ROOT — row id=456 has name=Gadget.
  Scenario: lookup a CSV column by key
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the client sends "GET" "/cat/456"
    Then the response status is 200
    And the response body is "name=Gadget"

  Scenario: the wait behavior delays the response past its floor
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the client sends "GET" "/wait"
    Then the response status is 200
    And the response body is "waited"
    And the response took at least 500 ms

  Scenario: shellTransform rewrites a static response body
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the client sends "GET" "/sh"
    Then the response status is 200
    And the response body is "transformed"

  # rift#272: a multi-value Set-Cookie must survive a copy behavior as two distinct headers (not
  # comma-folded). This build serves both, so the historical @gap is resolved — we assert the
  # correct behavior, which now guards against a regression. https://github.com/achird-labs/rift/issues/272
  Scenario: multi-value Set-Cookie survives a copy behavior (rift#272)
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the client sends "GET" "/cc?q=hi"
    Then the response status is 200
    And the response body is "x=hi"
    And the response has 2 "Set-Cookie" headers

  Scenario: a JS decorate behavior rewrites the response
    Given a fresh imposter from fixture "behaviors/behaviors"
    When the client sends "GET" "/dec"
    Then the response status is 200
    And the response body is "decorated"

  # Mountebank v2 inject parity (rift#438): the `inject` function is config-first — its single
  # argument IS the request, exposing method/path/body directly (mirroring Mountebank's convention).
  Scenario: a config-first inject reads the request method and path
    Given a fresh imposter from fixture "behaviors/mb-inject"
    When the client sends "GET" "/cfg"
    Then the response status is 200
    And the response body is "GET /cfg"

  Scenario: a config-first inject reads the request body
    Given a fresh imposter from fixture "behaviors/mb-inject"
    When the request body is payload-123
    And the client sends "POST" "/body"
    Then the response status is 200
    And the response body is "got:payload-123"

  # The inject function has a native `logger` (config.logger) — using it does not fail the response.
  Scenario: an inject function can use the native logger
    Given a fresh imposter from fixture "behaviors/mb-inject"
    When the client sends "GET" "/log"
    Then the response status is 200
    And the response body is "logged"

  # rift#1195 (0.18.0): a `behaviors` ARRAY runs every copy/lookup/shellTransform element in order, as
  # Mountebank 2.9.1 does. Before, the fold kept only the last element per key, so `${A}` stayed
  # literal. The imposter is the static corpus file 21, which the feature-01 gate also lints and
  # rift-verifies.
  Scenario: a behaviors array applies every copy element in order
    Given a fresh imposter from corpus file "21-behaviors-array-copy"
    When the client sends "GET" "/copy"
    Then the response status is 200
    And the response body is "A=cop B=GET"

  # Also rift#1195: a malformed EARLIER element (a copy with no `from`) is refused at the admin API
  # rather than loading with that copy silently dead.
  Scenario: a behaviors array with a malformed earlier copy element is refused at creation
    When the client posts the imposter fixture "behaviors/array-copy-malformed"
    Then the response status is 400
    And the response body contains "`copy` behavior is malformed"

  # rift#1198 (0.18.0): a `behaviors` ARRAY is an ordered program, run element by element as
  # Mountebank does. Before, it was folded into one object per key, so the last `decorate` or `wait`
  # won and the object form's fixed order applied.
  Scenario: a behaviors array runs two decorate elements in order
    Given a fresh imposter from fixture "behaviors/array-order"
    When the client sends "GET" "/decorate-twice"
    Then the response status is 200
    And the response body is "x12"

  # Only a lower bound: two 300 ms waits must both apply. No upper bound, so CI jitter cannot flake it.
  Scenario: a behaviors array runs two wait elements, delaying for both
    Given a fresh imposter from fixture "behaviors/array-order"
    When the client sends "GET" "/wait-twice"
    Then the response status is 200
    And the response took at least 600 ms

  # The decorate element runs FIRST and consumes `${T}`, so the later copy has no token to fill.
  Scenario: a behaviors array runs a decorate before a later copy
    Given a fresh imposter from fixture "behaviors/array-order"
    When the request header "X-T" is "COPY"
    And the client sends "GET" "/decorate-then-copy"
    Then the response status is 200
    And the response body is "DEC-WON"

  # The object form runs in Mountebank's fixed order, where shellTransform precedes decorate.
  Scenario: the object form runs shellTransform before decorate
    Given a fresh imposter from fixture "behaviors/array-order"
    When the client sends "GET" "/object-order"
    Then the response status is 200
    And the response body is "SD"

  # Object form: copy then lookup, and a copied value spelling a lookup token stays literal (rift#1203).
  Scenario: the object form serves a copied lookup token literally
    Given a fresh imposter from fixture "behaviors/array-order"
    When the request header "X-Q" is "${R}[price]"
    And the client sends "GET" "/object-copy-lookup?id=123"
    Then the response status is 200
    And the response body is "q=${R}[price] name=Widget"

  # rift#1199: a multi-item `copy` is written back by GET /imposters as one element per item. Only the
  # rift half is asserted; "a real mb accepts it" needs a Mountebank binary this harness does not run.
  Scenario: a multi-item copy is written back as one behaviors element per item
    Given a fresh imposter from fixture "behaviors/array-order"
    When the client sends "GET" "/multi-copy"
    Then the response body is "a=multi b=GET"
    And the current imposter writes stub 5 back with 2 single-item copy behaviors
