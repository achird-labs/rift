@rift @templating
Feature: Response templating — request variables and date tokens

  Scenario: request method and path variables render
    Given a fresh imposter from fixture "templating/templates"
    When the client sends "GET" "/rmp"
    Then the response status is 200
    And the response body is "m=GET p=/rmp"

  Scenario: request query variable renders
    Given a fresh imposter from fixture "templating/templates"
    When the client sends "GET" "/rq?name=bob"
    Then the response status is 200
    And the response body is "q=bob"

  Scenario: request header variable renders
    Given a fresh imposter from fixture "templating/templates"
    When the request header "X-Test" is "val"
    And the client sends "GET" "/rh"
    Then the response status is 200
    And the response body is "h=val"

  # Each date token must render an ISO-8601 date with the CORRECT offset — not just "some date" and
  # not the literal `{{...}}`. The expected date is computed at assertion time, so a bug that ignored
  # the offset/sign (e.g. always returned NOW) fails, while the test never goes stale.
  Scenario Outline: date token <token> renders a date <offset> <unit> from today
    Given a fresh imposter from fixture "templating/templates"
    When the client sends "GET" "<path>"
    Then the response status is 200
    And the response body is a date offset by <offset> <unit> from today

    Examples:
      | token    | path | offset | unit   |
      | NOW      | /now | 0      | days   |
      | DAYS+5   | /d5  | 5      | days   |
      | DAYS-5   | /dm5 | -5     | days   |
      | MONTHS+1 | /m1  | 1      | months |
      | MONTHS-1 | /mm1 | -1     | months |

  # rift 0.13.0 (rift#444): the richer `{{ ... }}` template FUNCTIONS (handlebars-style args) are opt-in
  # behind the `_rift.templated: true` response flag — distinct from the always-on `${...}` vars and the
  # `{{NOW}}`/`{{DAYS±N}}` date tokens above. `{{ request.header 'X-Name' }}` renders the request header.
  Scenario: a templated response renders a {{ }} request-field function into the body
    Given a fresh imposter from fixture "templating/templated"
    When the request header "X-Name" is "world"
    And the client sends "GET" "/hdr"
    Then the response status is 200
    And the response body is "hi world"

  # The flag is opt-in: WITHOUT `_rift.templated`, the same `{{ ... }}` is served verbatim (not rendered).
  Scenario: without the templated flag the {{ }} function is left literal
    Given a fresh imposter from fixture "templating/templated"
    When the request header "X-Name" is "world"
    And the client sends "GET" "/plain"
    Then the response status is 200
    And the response body is "hi {{ request.header 'X-Name' }}"

  # A second {{ }} function — randomInt with equal bounds renders deterministically.
  Scenario: a templated response renders the randomInt {{ }} function
    Given a fresh imposter from fixture "templating/templated"
    When the client sends "GET" "/rand"
    Then the response status is 200
    And the response body is "n=7"

  # rift 0.13.0 (rift#435): a stub's `routePattern` extracts path parameters into
  # `request.pathParams.<name>`, usable from a response template. (By engine design pathParams feeds
  # templates and scripts — not predicate matching — so ordinary predicates route to the stub.)
  Scenario Outline: routePattern extracts a path parameter into a response template
    Given a fresh imposter from fixture "templating/path-params"
    When the client sends "GET" "<path>"
    Then the response status is 200
    And the response body is "<body>"

    Examples:
      | path         | body       |
      | /users/42    | user=42    |
      | /users/alice | user=alice |

  # The same extraction is visible to a script via `ctx.request.pathParams`.
  Scenario: routePattern extracts a path parameter visible to a script
    Given a fresh imposter from fixture "templating/path-params"
    When the client sends "GET" "/s/77"
    Then the response status is 200
    And the response body is "script=77"

  # routePattern is NOT a predicate: a path that satisfies the stub's predicate (startsWith /users/)
  # but does not fit the pattern shape (/users/:id) still serves the stub — pathParams is simply empty.
  Scenario: a routePattern that the path does not fit still serves the stub with empty pathParams
    Given a fresh imposter from fixture "templating/path-params"
    When the client sends "GET" "/users/42/extra"
    Then the response status is 200
    And the response body is "user="

  # rift#1203 (fixed in 0.18.0): text substituted from the REQUEST is data, never re-scanned as a
  # template token. Each scenario sends a client value that spells a live token; the author's own
  # tokens still render (proving the pass ran), while the client's token is served literally.
  # products.csv row id=123 is name=Widget, price=9.99 — `price` stands in for the "secret" column.
  Scenario: a lookup token sent in the query is served literally while the author's lookup renders
    Given a fresh imposter from fixture "templating/no-reexpand"
    When the client sends "GET" "/lookup?id=123&q=%24%7BR%7D%5Bprice%5D"
    Then the response status is 200
    And the response body is "q=${R}[price] name=Widget"

  Scenario: a templated response serves a client-sent ${request.headers} token literally
    Given a fresh imposter from fixture "templating/no-reexpand"
    When the request header "x-internal" is "SECRET"
    And the client sends "GET" "/templated?q=%24%7Brequest.headers.x-internal%7D"
    Then the response status is 200
    And the response body is "q=${request.headers.x-internal}"

  # The client sends an UNCLOSED token; the author's closing `}` would complete it after substitution.
  Scenario: a partial token completed by the author's delimiter is served as sent
    Given a fresh imposter from fixture "templating/no-reexpand"
    When the request header "x-internal" is "SECRET"
    And the client sends "GET" "/partial?id=%24%7Brequest.headers.x-internal"
    Then the response status is 200
    And the response body is "{\"id\":${request.headers.x-internal}"

  # Deliberate divergence from Mountebank 2.9.1: there, a later `lookup` re-scans the body after an
  # earlier `copy`, so a client-sent `${R}[price]` would render as `9.99`. Rift treats copied request
  # text as data (rift#1203) and serves it literally.
  @rift-only
  Scenario: copy then lookup serves a client-sent lookup token literally
    Given a fresh imposter from fixture "templating/no-reexpand"
    When the request header "X-Q" is "${R}[price]"
    And the client sends "GET" "/copy-lookup?id=123"
    Then the response status is 200
    And the response body is "q=${R}[price] name=Widget"

  # ── the `_rift.templated` function grammar (rift docs/features/date-templates.md, #195) ────────

  Scenario: {{ uuid }} renders a fresh v4 UUID per request
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/uuid"
    Then the response status is 200
    And the response body matches "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
    When the client remembers the response body
    And the client sends "GET" "/uuid"
    Then the response body matches "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"
    And the response body differs from the remembered one

  Scenario: {{ now }} honours an offset and a strftime format
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/tomorrow"
    Then the response body matches "^\d{4}-\d{2}-\d{2}$"
    And the response body is a date offset by 1 "days" from today

  Scenario: {{ request.json }} selects a JSONPath out of the request body
    Given a fresh imposter from fixture "templating/functions"
    When the request body is "{\"items\":[{\"id\":1},{\"id\":2}]}"
    And the client sends "POST" "/json"
    Then the response body is "2"

  Scenario: a repeated query key renders comma-joined
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/tags?tag=a&tag=b"
    Then the response body is "a,b"

  Scenario Outline: the <filter> filter
    Given a fresh imposter from fixture "templating/functions"
    When the request header "X-Q" is "a\"b"
    And the client sends "GET" "<path>"
    Then the response body is "<body>"

    Examples:
      | filter       | path         | body   |
      | last_segment | /seg/a/b/    | b      |
      | regex        | /u/42        | 42     |
      | json         | /json-filter | a\\\"b |

  Scenario: a template renders into a response header value
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/header"
    Then the response header "X-Req" is "GET"

  # By default a failing token renders empty (with a rift::template warning) and the response serves.
  Scenario: an unknown template function renders as an empty substitution
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/unknown-fn"
    Then the response status is 200
    And the response body is "[]"
    And the response has no header "x-rift-template-error"

  @rift-debug-env
  Scenario: under --debug an unknown template function fails the response with a 500
    Given a fresh imposter from fixture "templating/functions" on an engine started with "--debug"
    When the client sends "GET" "/unknown-fn"
    Then the response status is 500
    And the response header "x-rift-template-error" is "true"
    And the response header "x-rift-imposter" is "true"
    And the error envelope message contains "nosuchfn"

  Scenario: a date token renders in a defaultResponse
    Given a fresh imposter from fixture "templating/functions"
    When the client sends "GET" "/no-stub-here"
    Then the response header "x-rift-default-response" is "true"
    And the response body is a date offset by 0 "days" from today

  Scenario: {{ state.<key> }} reads the default in-memory flow store
    Given a fresh imposter from fixture "templating/functions"
    When the client sets flow-state "k" in flow "f9" to "kv"
    And the request header "X-Flow" is "f9"
    And the client sends "GET" "/state"
    Then the response body is "k=kv"
