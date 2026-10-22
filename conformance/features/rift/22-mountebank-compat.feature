@rift @mbcompat
Feature: Mountebank compatibility — predicates, imposter options and response forms the corpus only loads

  # rift docs/mountebank/{predicates,imposters,responses,behaviors}.md (#203). The corpus LOADS most of
  # these but its `_verify` asserts none of them; each row here pins one literally. Runs on every
  # leg. Tags: `@loopback-client` rows match the client address 127.0.0.1, which a host-less
  # imposter reports IPv4-mapped (`::ffff:127.0.0.1`) as Mountebank's dual-stack listener does (#1514),
  # and which a Docker-published port rewrites (excluded in container). The lookup rows read a CSV under `corpus/data`, which every
  # mode reaches (the container mounts it at its workdir, #206); `@shell` rows need `sh` in the engine's
  # environment (the debian-slim image has it).

  # ── predicates ─────────────────────────────────────────────────────────────────────────────────

  Scenario: keyCaseSensitive makes query names case-sensitive; by default they are not
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "/kcs?q=v"
    Then the response header "x-rift-no-match" is "true"
    When the client sends "GET" "/kcs?Q=v"
    Then the response body is "kcs"
    When the client sends "GET" "/kci?q=v"
    Then the response body is "kci"

  Scenario: a form predicate matches an application/x-www-form-urlencoded body
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request header "Content-Type" is "application/x-www-form-urlencoded"
    And the request body is "a=1&b=2"
    And the client sends "POST" "/form"
    Then the response body is "form"

  @loopback-client
  Scenario Outline: the <field> request field carries the client address
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "<path>"
    Then the response body is "<body>"

    Examples:
      | field       | path  | body |
      | requestFrom | /from | from |
      | ip          | /ip   | ip   |

  Scenario: an xpath selector resolves a namespace prefix through ns
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "<r xmlns=\"urn:t\"><id>7</id></r>"
    And the client sends "POST" "/xpath"
    Then the response body is "xpath"

  Scenario: a jsonpath selector without the leading $ is root-relative
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "{\"user\":{\"name\":\"alice\"}}"
    And the client sends "POST" "/jsonpath"
    Then the response body is "jsonpath"

  Scenario: deepEquals refuses a request carrying an extra query field
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "/deep?a=1&b=2"
    Then the response header "x-rift-no-match" is "true"
    When the client sends "GET" "/deep?a=1"
    Then the response body is "deep"

  Scenario: exists false matches only when the header is absent
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "/exists"
    Then the response body is "exists-false"
    When the request header "X-Absent" is "1"
    And the client sends "GET" "/exists"
    Then the response header "x-rift-no-match" is "true"

  Scenario: or matches either branch; nested and/or/not compose
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "/or1"
    Then the response body is "or"
    When the client sends "GET" "/or2"
    Then the response body is "or"
    When the client sends "POST" "/nested"
    Then the response body is "nested"
    When the client sends "GET" "/nested"
    Then the response header "x-rift-no-match" is "true"

  Scenario: the legacy inject predicate form function(request, logger) matches
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the client sends "GET" "/li"
    Then the response body is "legacy-inject"

  # rift#1255 / #1257, Mountebank parity (pinned in conformance/differential too): jsonpath-plus
  # shorthands load, and a selector that selects several values matches when ANY of them holds.
  # The expected value sits at index 1 so only an open-ended slice finds it, and `c` is not a direct
  # child of `b` so only a descendant segment finds it.
  Scenario: a [:0] slice end is open-ended, so the whole array is selected
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "{\"x\":{\"y\":[{\"z\":\"second\"},{\"z\":\"first\"}]}}"
    And the client sends "POST" "/jp-slice"
    Then the response body is "jp-slice"

  Scenario: a .[ segment is a descendant segment
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "{\"a\":{\"b\":{\"q\":[{\"c\":\"deep\"}]}}}"
    And the client sends "POST" "/jp-desc"
    Then the response body is "jp-desc"

  Scenario: equals holds when any selected value equals; deepEquals compares the whole selection
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "{\"x\":{\"y\":[{\"z\":\"second\"},{\"z\":\"first\"}]}}"
    And the client sends "POST" "/jp-any"
    Then the response body is "jp-any"
    When the client sends "POST" "/jp-nodeep"
    Then the response header "x-rift-no-match" is "true"
    When the client sends "POST" "/jp-deep"
    Then the response body is "jp-deep"

  Scenario: an xpath selector over several nodes matches when any of them equals
    Given a fresh imposter from fixture "mbcompat/predicates"
    When the request body is "<users><user><name>alice</name></user><user><name>bob</name></user></users>"
    And the client sends "POST" "/xp-any"
    Then the response body is "xp-any"

  # ── imposter options ───────────────────────────────────────────────────────────────────────────

  Scenario: a miss is served the defaultResponse, marked x-rift-default-response
    Given a fresh imposter from fixture "mbcompat/options"
    When the client sends "GET" "/miss"
    Then the response status is 404
    And the response body is "nf"
    And the response header "x-rift-default-response" is "true"

  # Mountebank answers a miss 200 with an empty body; Rift adds the x-rift-no-match marker, never 404.
  Scenario: without a defaultResponse a miss is 200, empty, and marked x-rift-no-match
    Given a fresh imposter from fixture "mbcompat/no-default"
    When the client sends "GET" "/miss"
    Then the response status is 200
    And the response body is ""
    And the response header "x-rift-no-match" is "true"

  # Mountebank's baseHttpServer.js:39-51 (#1555): only an OPTIONS carrying Origin,
  # Access-Control-Request-Method and Access-Control-Request-Headers is a preflight. It is matched
  # like any request, and its three values are echoed onto the stub's answer.
  Scenario: allowCORS echoes a full preflight onto the stub's answer and adds nothing otherwise
    Given a fresh imposter from fixture "mbcompat/options"
    When the request header "Origin" is "http://a.test"
    And the client sends "GET" "/hit"
    Then the response body is "hit"
    And the response has no header "access-control-allow-origin"
    When the request header "Access-Control-Request-Method" is "POST"
    And the client sends "OPTIONS" "/hit"
    Then the response has no header "access-control-allow-origin"
    When the request header "Access-Control-Request-Headers" is "X-Custom"
    And the client sends "OPTIONS" "/hit"
    Then the response status is 200
    And the response body is "hit"
    And the response header "access-control-allow-origin" is "http://a.test"
    And the response header "access-control-allow-methods" is "POST"
    And the response header "access-control-allow-headers" is "X-Custom"

  Scenario: recordRequests false keeps the journal empty after traffic
    Given a fresh imposter from fixture "mbcompat/options"
    When the client sends "GET" "/hit"
    And the client sends admin "GET" "/imposters/{port}/savedRequests" with payload "-"
    Then the response status is 200
    And the response JSON at "0" is absent

  Scenario: name, serviceName and serviceInfo round-trip through the replayable export
    Given a fresh imposter from fixture "mbcompat/options"
    When the client sends admin "GET" "/imposters/{port}?replayable=true" with payload "-"
    Then the response status is 200
    And the response JSON at "name" is "\"mb-opts\""
    And the response JSON at "serviceName" is "\"svc\""
    And the response JSON at "serviceInfo" is "{\"team\":\"t1\",\"tier\":2}"

  # ── response forms ─────────────────────────────────────────────────────────────────────────────

  Scenario: a flat response with a string statusCode is served without an is wrapper
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/flat204"
    Then the response status is 204

  Scenario: an is wins over the flat fields of the same response
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/flat-and-is"
    Then the response status is 200
    And the response body is "is-wins"

  Scenario: an object body without a Content-Type is served as application/json
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/object"
    Then the response header "content-type" is "application/json"
    And the response body is "{\"a\":1}"

  # rift#1085: numbers keep their digits (7e+23, not 6.999999999999999e23). The exponent is written
  # `7e+23` since 0.20.0 (rift#1312), as JSON.stringify writes it.
  Scenario: a JSON body keeps its number digits
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/numbers"
    Then the response body contains "\"f\":0.1"
    And the response body contains "\"big\":7e+23"

  # DOCUMENTED DIVERGENCE (rift#1241, docs/mountebank/responses.md since 0.19.0): Mountebank serves
  # JSON.stringify(body), keeping the declared order ({"f":…,"big":…}); Rift key-sorts JSON values
  # throughout the engine (stable stub identity for incremental reload relies on it). Not a gap: a
  # client that needs exact bytes declares a string body. A change here is a behaviour change.
  Scenario: an object body is served with its keys sorted — a documented divergence (rift#1241)
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/numbers"
    Then the response body is "{\"big\":7e+23,\"f\":0.1}"

  Scenario: ${request.body} interpolates the request body
    Given a fresh imposter from fixture "mbcompat/responses"
    When the request body is "hello"
    And the client sends "POST" "/echo"
    Then the response body is "got hello"

  Scenario: ${request.pathParams.x} interpolates a routePattern parameter
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/users/42"
    Then the response body is "id=42"

  Scenario: a mixed is/inject response cycle alternates, and the inject turn is marked
    Given a fresh imposter from fixture "mbcompat/responses"
    When the client sends "GET" "/mixed"
    Then the response body is "A"
    And the response has no header "x-rift-inject"
    When the client sends "GET" "/mixed"
    Then the response body is "B"
    And the response header "x-rift-inject" is "true"
    When the client sends "GET" "/mixed"
    Then the response body is "A"

  # ── behaviors ──────────────────────────────────────────────────────────────────────────────────

  Scenario: copy using xpath
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the request body is "<r><id>9</id></r>"
    And the client sends "POST" "/copy-xpath"
    Then the response body is "v=9"

  # Mountebank parity: copy/lookup resolve a prefix through `using.ns` by namespace URI, not prefix text.
  Scenario: copy using xpath resolves a prefix through ns by URI
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>9</a:id></r>"
    And the client sends "POST" "/copy-xpath-ns"
    Then the response body is "v=9"
    When the request body is "<r xmlns:z=\"urn:a\"><z:id>8</z:id></r>"
    And the client sends "POST" "/copy-xpath-ns"
    Then the response body is "v=8"

  Scenario: lookup keyed by an xpath resolves a prefix through ns
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>cd</a:id></r>"
    And the client sends "POST" "/lookup-xpath-ns"
    Then the response body is "n=second"

  # A selector prefix with no `ns` binding fails the request, as in Mountebank 2.9.5, even though the
  # document declares it (issue #1585; it used to select nothing and leave the token).
  Scenario: copy using xpath with an unbound prefix fails the request
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>9</a:id></r>"
    And the client sends "POST" "/copy-xpath-unbound"
    Then the response status is 500
    And the error envelope message contains "malformed xpath predicate selector"

  # The regex returns the first capture group (a documented Mountebank divergence: MB returns the
  # whole match), and options.ignoreCase lets /COPY-REGEX/ match /copy-regex/.
  Scenario: copy using regex with ignoreCase yields the first capture group
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/copy-regex/77"
    Then the response body is "v=77"

  Scenario: lookup honours a csv delimiter
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/lookup/cd"
    Then the response body is "n=second"

  # rift#1240 (fixed in 0.19.0): a lookup key's `index` picks the regex capture group, as in
  # Mountebank — `index: 2` keys the row on the second group (`cd` → "second"). Up to 0.18.1 the
  # index was dropped and the first group keyed it (`ab` → "first").
  Scenario: a lookup key index selects the capture group (rift#1240)
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/lookup-index/ab-cd"
    Then the response body is "n=second"

  @shell
  Scenario: shellTransform receives the request in MB_REQUEST
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/shell-env"
    Then the response status is 200
    And the response body contains "\"method\":\"GET\""

  @shell
  Scenario: a failing shellTransform serves the original body marked x-rift-shelltransform-error
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/shell-fail"
    Then the response status is 200
    And the response body is "orig"
    And the response header "x-rift-shelltransform-error" is "true"

  # rift#1589: stdout must be a JSON response, as in Mountebank; `echo hello` also prints the two
  # appended JSON arguments, so it is invalid JSON there too. Lenient: the original, marked.
  @shell
  Scenario: a shellTransform that prints plain text fails and serves the original marked
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/shell-text"
    Then the response status is 200
    And the response body is "orig"
    And the response header "x-rift-shelltransform-error" is "true"

  Scenario: decorate receives a string response.body
    Given a fresh imposter from fixture "mbcompat/behaviors"
    When the client sends "GET" "/decorate"
    Then the response body is "type=string"
