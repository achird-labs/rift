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

  # rift 0.13.0 (rift#498): when a proxy's predicateGenerators.inject THROWS during recording, rift
  # tags the proxied response with `x-rift-generator-error: script-error` and SKIPS recording the
  # shadowing match-all stub (previously it recorded a match-all that would shadow the real upstream
  # on replay). The proxy still forwards the upstream response — only the unsafe recording is
  # suppressed — so the stub count stays at the single proxy stub.
  Scenario: a failing predicateGenerators.inject tags the response and records no shadowing stub
    Given a fresh verbatim imposter from fixture "proxy/generror-upstream"
    And a fresh verbatim imposter from fixture "proxy/generror-proxy"
    Then the imposter on port 4819 has 1 stubs
    When the client sends "GET" "/svc" to port 4819
    Then the response status is 200
    And the response body is "UP"
    And the response header "x-rift-generator-error" is "script-error"
    And the imposter on port 4819 still has 1 stubs after recording settles

  # ── proxy options (rift docs/mountebank/proxy.md, #202) ─────────────────────────────────────────
  # Each scenario proxies to a recording upstream imposter on a fresh pool port (`UPSTREAM_PORT` in
  # the fixture), so the forwarded request can be read back from the upstream's journal.

  # proxyAlways forwards every request and records one stub per unique request, inserted right after
  # the proxy stub (so the newest sits first) and merging repeat responses into the existing stub.
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
    And the response JSON at "stubs.1.predicates.0.equals.path" is "\"/y\""
    And the response JSON at "stubs.1.responses" has 1 elements
    And the response JSON at "stubs.2.predicates.0.equals.path" is "\"/x\""
    And the response JSON at "stubs.2.responses" has 2 elements

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

  # The proxy TLS client fields are accepted and dropped on load: they do not read back.
  Scenario: the proxy key/cert/passphrase/ciphers fields are accepted and not read back
    Given an upstream imposter from fixture "proxy/options/upstream"
    And a fresh imposter from fixture "proxy/options/key-cert" proxying to the upstream
    When the client sends admin "GET" "/imposters/{port}?replayable=true" with payload "-"
    Then the response status is 200
    And the response JSON at "stubs.0.responses.0.proxy.to" is present
    And the response JSON at "stubs.0.responses.0.proxy.key" is absent
    And the response JSON at "stubs.0.responses.0.proxy.cert" is absent
    And the response JSON at "stubs.0.responses.0.proxy.passphrase" is absent
    And the response JSON at "stubs.0.responses.0.proxy.ciphers" is absent
