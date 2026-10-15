@rift @smoke
Feature: Plane A smoke — the harness posts a fresh imposter and drives it directly

  Scenario: a fresh imposter answers a GET with its stubbed body
    Given a fresh imposter from fixture "smoke/health"
    When the client sends "GET" "/health"
    Then the response status is 200
    And the response body is "OK"

  # The imposter's stub only matches when the staged header AND body reach the engine, so a green
  # here proves request staging is wired correctly (a plumbing bug would fall through to the empty
  # default response and fail the body assertion — not misattribute to the engine).
  Scenario: a staged request header and body reach the imposter
    Given a fresh imposter from fixture "smoke/echo"
    When the request header "X-Rift-Test" is "1"
    And the request body is "ping"
    And the client sends "POST" "/echo"
    Then the response status is 200
    And the response body contains "staged"
