@rift @core
Feature: Core serving — predicates, cycling, copy, FSM, headers, templates, forward, gateway

  # Stateless per-request predicate checks — one row per predicate kind. Each Example is its own
  # scenario (fresh imposter), so this is safe to tabulate.
  Scenario Outline: predicate <kind> selects the stub
    Given a fresh imposter from fixture "core/predicates"
    When the request body is <body>
    And the client sends "<method>" "<path>"
    Then the response status is 200
    And the response body is "<expect>"

    Examples:
      | kind          | method | path    | body            | expect |
      | not+startsWith | GET    | /open/x |                 | nsw    |
      | matches       | GET    | /id/42  |                 | re     |
      | jsonpath      | POST   | /body   | {"a":"x"}       | jp     |
      | xpath         | POST   | /body   | <r><a>v</a></r> | xp     |

  # Stateful: response cycling is a single ordered sequence (repeat:2 → then 200 → wrap to first),
  # so it must stay one scenario — an Outline would reset the imposter between rows.
  Scenario: response cycling with repeat:N wraps around
    Given a fresh imposter from fixture "core/cycling"
    When the client sends "GET" "/r"
    Then the response status is 503
    When the client sends "GET" "/r"
    Then the response status is 503
    When the client sends "GET" "/r"
    Then the response status is 200
    When the client sends "GET" "/r"
    Then the response status is 503

  Scenario: copy path regex group into the response body
    Given a fresh imposter from fixture "core/copy-path"
    When the client sends "GET" "/orders/77"
    Then the response status is 200
    And the response body is "id=77"

  Scenario: FSM two-step state transition
    Given a fresh imposter from fixture "core/fsm"
    When the client sends "GET" "/go"
    Then the response status is 201
    And the response body is "first"
    When the client sends "GET" "/go"
    Then the response status is 200
    And the response body is "second"

  Scenario: multi-value Set-Cookie headers are preserved
    Given a fresh imposter from fixture "core/cookies"
    When the client sends "GET" "/h"
    Then the response status is 200
    And the response has 2 "Set-Cookie" headers

  Scenario: date template renders to an ISO-8601 date
    Given a fresh imposter from fixture "core/date-template"
    When the client sends "GET" "/d"
    Then the response status is 200
    And the response body matches "^[0-9]{4}-[0-9]{2}-[0-9]{2}T"

  Scenario: defaultForward proxies unmatched requests to another imposter
    Given a fresh verbatim imposter from fixture "core/forward-target"
    And a fresh verbatim imposter from fixture "core/forwarder"
    When the client sends "GET" "/fwd"
    Then the response status is 200
    And the response body is "forwarded"

  Scenario: single-port gateway routes to an imposter by port
    Given a fresh imposter from fixture "core/gateway"
    When the client sends "GET" "/deep/path" via the gateway
    Then the response status is 200
    And the response body is "gw"

  # rift 0.13.0 (rift#480) narrowed case-insensitive contains/startsWith/endsWith from Unicode folding
  # to ASCII-only (matching equals). Each predicate value carries an accented letter: the ASCII part
  # still folds (CAF→caf), but É/é and Ï/ï no longer fold, so an accent-case-swapped body falls through
  # to the match-all fallback ("no"). One fresh imposter per row (core/case-fold), so an Outline is safe.
  Scenario Outline: case-insensitive <kind> folds ASCII but not accented letters
    Given a fresh imposter from fixture "core/case-fold"
    When the request body is <body>
    And the client sends "POST" "/"
    Then the response status is 200
    And the response body is "<expect>"

    Examples:
      | kind                 | body          | expect |
      | startsWith ASCII     | CAFéteria     | sw     |
      | startsWith accent    | cafÉteria     | no     |
      | contains ASCII       | the NAïVE cat | co     |
      | contains accent      | the NAÏVE cat | no     |
      | endsWith ASCII       | grand-élAN    | ew     |
      | endsWith accent      | grand-Élan    | no     |
