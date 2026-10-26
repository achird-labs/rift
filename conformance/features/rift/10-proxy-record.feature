@rift @transport
Feature: Proxy record/replay — proxyOnce, proxyTransparent, proxyAlways chains, and predicateGenerators

  Scenario: a proxyOnce proxy with predicateGenerators records a replayable stub, then replays it
    Given a fresh verbatim imposter from fixture "proxy/once-upstream"
    And a fresh verbatim imposter from fixture "proxy/once-proxy"
    Then the imposter on port 4814 has 1 stubs
    When the client sends "GET" "/svc" to port 4814
    Then the response status is 200
    And the response body is "UP"
    And the imposter on port 4814 has 2 stubs
    And the imposter on port 4814 first recorded predicate includes "/svc"
    And the imposter on port 4814 first recorded predicate includes "GET"
    When the client tears down the imposter on port 4813
    And the client sends "GET" "/svc" to port 4814
    Then the response status is 200
    And the response body is "UP"

  Scenario: a proxyOnce proxy WITHOUT predicateGenerators does not record a replayable stub
    Given a fresh verbatim imposter from fixture "proxy/nopg-upstream"
    And a fresh verbatim imposter from fixture "proxy/nopg-proxy"
    Then the imposter on port 4816 has 1 stubs
    When the client sends "GET" "/svc" to port 4816
    Then the response status is 200
    And the response body is "UP2"
    And the imposter on port 4816 still has 1 stubs after recording settles

  Scenario: a proxyTransparent proxy forwards the response but records nothing
    Given a fresh verbatim imposter from fixture "proxy/transparent-upstream"
    And a fresh verbatim imposter from fixture "proxy/transparent-proxy"
    Then the imposter on port 4800 has 1 stubs
    When the client sends "GET" "/t" to port 4800
    Then the response status is 200
    And the response body is "TP"
    And the imposter on port 4800 still has 1 stubs after recording settles

  Scenario: a multi-level proxyAlways chain resolves through every hop
    Given a fresh verbatim imposter from fixture "proxy/chain-leaf"
    And a fresh verbatim imposter from fixture "proxy/chain-mid"
    And a fresh verbatim imposter from fixture "proxy/chain-top"
    When the client sends "GET" "/c" to port 4803
    Then the response status is 200
    And the response body is "leaf"

  Scenario: predicateGenerators can capture the query string
    Given a fresh verbatim imposter from fixture "proxy/query-upstream"
    And a fresh verbatim imposter from fixture "proxy/query-proxy"
    When the client sends "GET" "/q?x=1" to port 4806
    Then the response status is 200
    And the response body is "Q"
    And the imposter on port 4806 has 2 stubs
    And the imposter on port 4806 first recorded predicate includes "/q"
    # Prove the query VALUE was captured, not merely the "query" key.
    And the imposter on port 4806 first recorded predicate includes "{\"x\":\"1\"}"

  # predicateGenerators capture a named request header into the recorded predicate. NOTE the config
  # shape: unlike path/method/query (which take a boolean `true`), `headers` takes a per-header
  # OBJECT — `"headers": {"X-Trace": true}`. rift does NOT honour mountebank's blanket `"headers":
  # true` (capture-all) shorthand — that form is silently ignored — which is the one genuine
  # mountebank-compat divergence here (a config-shape gap, not a capture bug).
  Scenario: predicateGenerators capture a named request header into the recorded predicate
    Given a fresh verbatim imposter from fixture "proxy/headers-upstream"
    And a fresh verbatim imposter from fixture "proxy/headers-proxy"
    When the request header "X-Trace" is "abc123"
    And the client sends "GET" "/h" to port 4811
    Then the response status is 200
    And the response body is "H"
    And the imposter on port 4811 has 2 stubs
    And the imposter on port 4811 first recorded predicate includes "/h"
    And the imposter on port 4811 first recorded predicate includes "X-Trace"
    And the imposter on port 4811 first recorded predicate includes "abc123"

  # rift#498: when a proxy's predicateGenerators.inject THROWS during recording, rift SKIPS recording
  # the shadowing match-all stub (previously it recorded a match-all that would shadow the real
  # upstream on replay), so the stub count stays at the single proxy stub. Since rift#1708 the request
  # is still forwarded but answered Mountebank's `500 invalid predicateGenerator injection`, with
  # `x-rift-generator-error: script-error` kept as a marker. The fixture's inject returns `[]` on
  # `config.request.isDryRun`: since rift#1580 one that throws there is refused at creation, as in
  # Mountebank's dry run.
  Scenario: a failing predicateGenerators.inject answers 500 and records no shadowing stub
    Given a fresh verbatim imposter from fixture "proxy/generror-upstream"
    And a fresh verbatim imposter from fixture "proxy/generror-proxy"
    Then the imposter on port 4819 has 1 stubs
    When the client sends "GET" "/svc" to port 4819
    Then the response status is 500
    And the response header "x-rift-generator-error" is "script-error"
    And the imposter on port 4819 still has 1 stubs after recording settles

  # ── proxy options (rift docs/mountebank/proxy.md, #202) ─────────────────────────────────────────
  # Each scenario proxies to a recording upstream imposter on a fresh pool port (`UPSTREAM_PORT` in
  # the fixture), so the forwarded request can be read back from the upstream's journal.

  # proxyAlways forwards every request and records one stub per unique request, appended to the end
  # of the stub list in the order the requests were made, and merging repeat responses into the
  # existing stub — Mountebank's recordProxyAlways (`stubs.add`), since rift#1389.
  Scenario: proxyAlways records one stub per unique request after the proxy, merging repeats
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/always" proxying to the upstream
    When the client sends "GET" "/x"
    Then the response status is 200
    And the response body is "up"
    And the response header "x-rift-proxy" is "true"
    When the client sends "GET" "/x"
    And the client sends "GET" "/y"
    Then the upstream imposter has recorded 3 requests
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs" has 3 elements
    And the response JSON at "stubs.0.responses.0.proxy.mode" is "\"proxyAlways\""
    And the response JSON at "stubs.1.predicates.0.deepEquals.path" is "\"/x\""
    And the response JSON at "stubs.1.responses" has 2 elements
    And the response JSON at "stubs.2.predicates.0.deepEquals.path" is "\"/y\""
    And the response JSON at "stubs.2.responses" has 1 elements

  # The upstream waits 300 ms; the live response reports the measured latency and the recorded stub
  # (proxyOnce records BEFORE the proxy, at index 0) carries it as a wait behavior.
  Scenario: addWaitBehavior reports the upstream latency and records it as a wait
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/add-wait" proxying to the upstream
    When the client sends "GET" "/slow"
    Then the response status is 200
    And the response body is "slow"
    And the response header "x-rift-proxy" is "true"
    And the response header "x-rift-proxy-latency" is a number at least 300
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs" has 2 elements
    And the response JSON at "stubs.0.responses.0.behaviors.0.wait" is a number at least 300

  Scenario: injectHeaders and pathRewrite shape the forwarded request
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/inject-rewrite" proxying to the upstream
    When the client sends "GET" "/api/users"
    Then the response status is 200
    And the response header "x-rift-proxy" is "true"
    And the upstream imposter's last recorded request has path "/users"
    And the upstream imposter's last recorded request has header "X-Inj" with value "v"

  # rift#1050 (a divergence from Mountebank, which would forward both): a case-variant repeat in a
  # single-valued header object is refused at create.
  Scenario: injectHeaders naming one header twice is refused
    When the client posts the imposter fixture "proxy/options/inject-dup"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "header `x-a` is already given as `X-A`"

  # Without predicateGenerators a proxyOnce proxy replays from its proxy store; clearing it sends the
  # next request upstream again.
  Scenario: DELETE savedProxyResponses clears a proxyOnce store so the upstream is reached again
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-store" proxying to the upstream
    When the client sends "GET" "/a"
    And the client sends "GET" "/a"
    Then the upstream imposter has recorded 1 requests
    When the client sends admin "DELETE" "/imposters/{port}/savedProxyResponses" with payload "-"
    Then the response status is 200
    When the client sends "GET" "/a"
    Then the response body is "up"
    And the upstream imposter has recorded 2 requests

  # rift#1239 (fixed in 0.19.0): the route also removes the stubs a proxy recorded through
  # predicateGenerators (they carry `recordedFrom`), as Mountebank's deleteSavedProxyResponses does,
  # and keeps the declared proxy stub. Up to 0.18.1 the recorded stub stayed and kept replaying.
  Scenario: DELETE savedProxyResponses removes a predicateGenerators-recorded stub (rift#1239)
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-pg" proxying to the upstream
    When the client sends "GET" "/z"
    Then the upstream imposter has recorded 1 requests
    When the client sends admin "DELETE" "/imposters/{port}/savedProxyResponses" with payload "-"
    Then the response status is 200
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs" has 1 elements
    And the response JSON at "stubs.0.recordedFrom" is absent
    When the client sends "GET" "/z"
    Then the response body is "up"
    And the upstream imposter has recorded 2 requests

  # The proxy's client identity (#1609): `key`/`cert`/`passphrase` are presented to an upstream
  # that asks for a client certificate, and read back as posted, as Mountebank prints them.
  # `ciphers` (an OpenSSL cipher list) is not modelled and is dropped.
  Scenario: the proxy key/cert/passphrase fields read back and ciphers is dropped
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/key-cert" proxying to the upstream
    When the client sends admin "GET" "/imposters/{port}?replayable=true" with payload "-"
    Then the response status is 200
    And the response JSON at "stubs.0.responses.0.proxy.to" is present
    And the response JSON at "stubs.0.responses.0.proxy.key" is present
    And the response JSON at "stubs.0.responses.0.proxy.cert" is present
    And the response JSON at "stubs.0.responses.0.proxy.passphrase" is the string "rift-test"
    And the response JSON at "stubs.0.responses.0.proxy.ciphers" is absent

  # Rift-only (documented in docs/mountebank/differences.md#proxy-client-certificates): Mountebank
  # admits half an identity and answers every proxied request 500; Rift refuses it at creation.
  Scenario: a proxy cert without its key is refused at creation
    When the client posts the imposter fixture "proxy/options/key-cert-unpaired"
    Then the response status is 400
    And the error envelope type is "bad data"
    And the error envelope message contains "proxy.cert is given without proxy.key"

  # ── predicateGenerators: jsonpath / xpath / ignore (#1327, #1333) ──────────────────────────────
  # Rift-only (documented in docs/mountebank/proxy.md): Mountebank copies `jsonpath`/`xpath` onto
  # every generated predicate, so its recordings never match and it forwards every request; for
  # `ignore` it records `deepEquals` without the ignored key and does not persist `ignore`, so its
  # recording never matches either. Rift records `equals` over the selected value / kept query names.
  Scenario: a jsonpath predicateGenerator replays on the selected value only
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-jsonpath" proxying to the upstream
    When the request body is "{\"id\":1,\"ts\":\"a\"}"
    And the client sends "POST" "/j"
    Then the upstream imposter has recorded 1 requests
    When the request body is "{\"id\":1,\"ts\":\"b\"}"
    And the client sends "POST" "/j"
    Then the upstream imposter has recorded 1 requests
    And the response has no header "x-rift-proxy"
    When the request body is "{\"id\":2,\"ts\":\"a\"}"
    And the client sends "POST" "/j"
    Then the upstream imposter has recorded 2 requests
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs.0.predicates.0.jsonpath.selector" is the string "$.id"

  Scenario: an xpath predicateGenerator with ns replays on the selected element only
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-xpath-ns" proxying to the upstream
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>7</a:id><ts>a</ts></r>"
    And the client sends "POST" "/x"
    Then the upstream imposter has recorded 1 requests
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>7</a:id><ts>b</ts></r>"
    And the client sends "POST" "/x"
    Then the upstream imposter has recorded 1 requests
    And the response has no header "x-rift-proxy"
    When the request body is "<r xmlns:a=\"urn:a\"><a:id>8</a:id><ts>a</ts></r>"
    And the client sends "POST" "/x"
    Then the upstream imposter has recorded 2 requests

  Scenario: ignore.query keeps a varying query name from splitting the recording
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-ignore" proxying to the upstream
    When the client sends "GET" "/i?q=rust&ts=1"
    Then the upstream imposter has recorded 1 requests
    When the client sends "GET" "/i?q=rust&ts=2"
    Then the upstream imposter has recorded 1 requests
    And the response has no header "x-rift-proxy"
    When the client sends "GET" "/i?q=go&ts=3"
    Then the upstream imposter has recorded 2 requests
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs.0.predicates.1.equals.query.q" is the string "rust"
    And the response JSON at "stubs.0.predicates.1.equals.query.ts" is absent

  # rift#1329 / rift#1333: Mountebank parity (both engines agree). `except` is persisted on the recorded
  # predicate and applied at match time; a whole-query generator records `deepEquals`, which refuses an
  # extra parameter; a request that missed every recording is forwarded and recorded (here keyed on body).
  Scenario: an except predicateGenerator replays a different id from the recording (rift#1329)
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-except" proxying to the upstream
    When the client sends "GET" "/users/123"
    Then the upstream imposter has recorded 1 requests
    When the client sends "GET" "/users/456"
    Then the response body is "up"
    And the response has no header "x-rift-proxy"
    And the upstream imposter has recorded 1 requests
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs.0.predicates.0.except" is the string "\\d+"

  Scenario: a recorded query predicate is deepEquals, so an extra parameter is forwarded again (rift#1329)
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-query" proxying to the upstream
    When the client sends "GET" "/q?a=1"
    Then the upstream imposter has recorded 1 requests
    When the client sends "GET" "/q?a=1"
    Then the upstream imposter has recorded 1 requests
    When the client sends "GET" "/q?a=1&b=2"
    Then the upstream imposter has recorded 2 requests
    And the response header "x-rift-proxy" is "true"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs" has 3 elements

  Scenario: a body predicateGenerator records on a miss and replays a repeat (rift#1333)
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/once-body" proxying to the upstream
    When the request body is "one"
    And the client sends "POST" "/b"
    Then the upstream imposter has recorded 1 requests
    When the request body is "two"
    And the client sends "POST" "/b"
    Then the upstream imposter has recorded 2 requests
    And the response header "x-rift-proxy" is "true"
    And the response body is "up"
    When the request body is "two"
    And the client sends "POST" "/b"
    Then the upstream imposter has recorded 2 requests
    And the response has no header "x-rift-proxy"
    When the client sends admin "GET" "/imposters/{port}" with payload "-"
    Then the response JSON at "stubs" has 3 elements

  # ── rift-lint ───────────────────────────────────────────────────────────────────────────────────
  # W020/W021 have unit coverage in rift-lint's validator tests; these rows pin the released binary.

  Scenario: rift-lint reports W020 for a predicateGenerator key the engine does not read
    Then rift-lint reports "W020" for fixture "proxy/lint-generator-unread-key"

  Scenario: rift-lint reports W021 for a predicateGenerator jsonpath that is not an object
    Then rift-lint reports "W021" for fixture "proxy/lint-generator-bad-jsonpath"

  Scenario: rift-lint raises no W020 or W021 for Mountebank's jsonpath generator form
    Then rift-lint reports no "W020" for fixture "proxy/lint-generator-clean"
    And rift-lint reports no "W021" for fixture "proxy/lint-generator-clean"
