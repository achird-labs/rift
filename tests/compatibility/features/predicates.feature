Feature: Predicate Matching Compatibility
  Rift should match requests using predicates identically to Mountebank

  Background:
    Given both Mountebank and Rift services are running
    And all imposters are cleared

  # ==========================================================================
  # Equals Predicate
  # ==========================================================================

  Scenario: Equals predicate matches exact path
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/exact/path"}}],
        "responses": [{"is": {"statusCode": 200, "body": "matched"}}]
      }
      """
    When I send GET request to "/exact/path" on imposter 4545
    Then both services should return status 200
    And both responses should have body "matched"

  Scenario: Equals predicate does not match different path
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404, "body": "not found"},
        "stubs": [{
          "predicates": [{"equals": {"path": "/exact/path"}}],
          "responses": [{"is": {"statusCode": 200, "body": "matched"}}]
        }]
      }
      """
    When I send GET request to "/different/path" on imposter 4545
    Then both services should return status 404

  Scenario: Equals predicate matches method
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"method": "POST"}}],
        "responses": [{"is": {"statusCode": 201, "body": "created"}}]
      }
      """
    When I send POST request to "/any" on imposter 4545
    Then both services should return status 201

  Scenario: Equals predicate matches headers
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"headers": {"X-Custom": "value"}}}],
        "responses": [{"is": {"statusCode": 200, "body": "header matched"}}]
      }
      """
    When I send GET request with header "X-Custom: value" on imposter 4545
    Then both services should return status 200
    And both responses should have body "header matched"

  Scenario: Equals predicate matches query parameters
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"query": {"key": "value"}}}],
        "responses": [{"is": {"statusCode": 200, "body": "query matched"}}]
      }
      """
    When I send GET request to "/?key=value" on imposter 4545
    Then both services should return status 200
    And both responses should have body "query matched"

  Scenario: Equals predicate matches request body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"body": "exact body"}}],
        "responses": [{"is": {"statusCode": 200, "body": "body matched"}}]
      }
      """
    When I send POST request with body "exact body" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Contains Predicate
  # ==========================================================================

  Scenario: Contains predicate matches substring in path
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"contains": {"path": "search"}}],
        "responses": [{"is": {"statusCode": 200, "body": "found"}}]
      }
      """
    When I send GET request to "/api/search/query" on imposter 4545
    Then both services should return status 200
    And both responses should have body "found"

  Scenario: Contains predicate matches substring in body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"contains": {"body": "needle"}}],
        "responses": [{"is": {"statusCode": 200, "body": "found needle"}}]
      }
      """
    When I send POST request with body "haystack needle haystack" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # StartsWith Predicate
  # ==========================================================================

  Scenario: StartsWith predicate matches path prefix
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"startsWith": {"path": "/api/v1"}}],
        "responses": [{"is": {"statusCode": 200, "body": "v1 api"}}]
      }
      """
    When I send GET request to "/api/v1/users" on imposter 4545
    Then both services should return status 200
    And both responses should have body "v1 api"

  Scenario: StartsWith does not match non-prefix
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404, "body": "not found"},
        "stubs": [{
          "predicates": [{"startsWith": {"path": "/api/v1"}}],
          "responses": [{"is": {"statusCode": 200}}]
        }]
      }
      """
    When I send GET request to "/other/api/v1" on imposter 4545
    Then both services should return status 404

  # ==========================================================================
  # EndsWith Predicate
  # ==========================================================================

  Scenario: EndsWith predicate matches path suffix
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"endsWith": {"path": ".json"}}],
        "responses": [{"is": {"statusCode": 200, "body": "json response"}}]
      }
      """
    When I send GET request to "/data/users.json" on imposter 4545
    Then both services should return status 200
    And both responses should have body "json response"

  # ==========================================================================
  # Matches (Regex) Predicate
  # ==========================================================================

  Scenario: Matches predicate with regex pattern
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"matches": {"path": "^/users/[0-9]+$"}}],
        "responses": [{"is": {"statusCode": 200, "body": "user found"}}]
      }
      """
    When I send GET request to "/users/123" on imposter 4545
    Then both services should return status 200
    And both responses should have body "user found"

  Scenario: Matches predicate does not match non-matching string
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404, "body": "not found"},
        "stubs": [{
          "predicates": [{"matches": {"path": "^/users/[0-9]+$"}}],
          "responses": [{"is": {"statusCode": 200}}]
        }]
      }
      """
    When I send GET request to "/users/abc" on imposter 4545
    Then both services should return status 404

  Scenario: Matches predicate with complex regex
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"matches": {"path": "^/api/(v[0-9]+)/.*"}}],
        "responses": [{"is": {"statusCode": 200, "body": "versioned api"}}]
      }
      """
    When I send GET request to "/api/v2/resource" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Exists Predicate
  # ==========================================================================

  Scenario: Exists predicate matches when header present
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"exists": {"headers": {"Authorization": true}}}],
        "responses": [{"is": {"statusCode": 200, "body": "authorized"}}]
      }
      """
    When I send GET request with header "Authorization: Bearer token" on imposter 4545
    Then both services should return status 200

  Scenario: Exists predicate with false checks absence
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"exists": {"headers": {"X-Debug": false}}}],
        "responses": [{"is": {"statusCode": 200, "body": "no debug"}}]
      }
      """
    When I send GET request to "/" on imposter 4545
    Then both services should return status 200
    And both responses should have body "no debug"

  # ==========================================================================
  # Compound Predicates
  # ==========================================================================

  Scenario: AND predicate requires all conditions
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "and": [
            {"equals": {"method": "POST"}},
            {"equals": {"path": "/api"}},
            {"exists": {"headers": {"Content-Type": true}}}
          ]
        }],
        "responses": [{"is": {"statusCode": 201}}]
      }
      """
    When I send POST request to "/api" with header "Content-Type: application/json" on imposter 4545
    Then both services should return status 201

  Scenario: OR predicate matches any condition
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "or": [
            {"equals": {"path": "/path1"}},
            {"equals": {"path": "/path2"}}
          ]
        }],
        "responses": [{"is": {"statusCode": 200, "body": "matched"}}]
      }
      """
    When I send GET request to "/path2" on imposter 4545
    Then both services should return status 200
    And both responses should have body "matched"

  Scenario: NOT predicate inverts match
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "not": {"equals": {"path": "/excluded"}}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "allowed"}}]
      }
      """
    When I send GET request to "/other" on imposter 4545
    Then both services should return status 200
    And both responses should have body "allowed"

  # ==========================================================================
  # Case Sensitivity
  # ==========================================================================

  Scenario: Case insensitive matching with caseSensitive false
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"path": "/API/Users"},
          "caseSensitive": false
        }],
        "responses": [{"is": {"statusCode": 200}}]
      }
      """
    When I send GET request to "/api/users" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Multiple Predicates (Implicit AND)
  # ==========================================================================

  Scenario: Multiple predicates act as AND
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [
          {"equals": {"method": "GET"}},
          {"startsWith": {"path": "/api/"}}
        ],
        "responses": [{"is": {"statusCode": 200}}]
      }
      """
    When I send GET request to "/api/resource" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # DeepEquals Predicate
  # ==========================================================================

  Scenario: DeepEquals predicate matches nested objects
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "deepEquals": {
            "query": {"filter": "active", "sort": "name"}
          }
        }],
        "responses": [{"is": {"statusCode": 200, "body": "deep matched"}}]
      }
      """
    When I send GET request to "/?filter=active&sort=name" on imposter 4545
    Then both services should return status 200
    And both responses should have body "deep matched"

  Scenario: DeepEquals fails with extra fields
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404},
        "stubs": [{
          "predicates": [{
            "deepEquals": {
              "query": {"filter": "active"}
            }
          }],
          "responses": [{"is": {"statusCode": 200}}]
        }]
      }
      """
    When I send GET request to "/?filter=active&extra=field" on imposter 4545
    Then both services should return status 404

  Scenario: DeepEquals matches JSON body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "deepEquals": {
            "body": {"user": {"name": "john", "age": 30}}
          }
        }],
        "responses": [{"is": {"statusCode": 200, "body": "json matched"}}]
      }
      """
    When I send POST request with JSON body '{"user": {"name": "john", "age": 30}}' on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Except Parameter
  # ==========================================================================

  Scenario: Except parameter strips pattern before matching
    # Note: ^/v[0-9]+/ strips "/v1/" including trailing slash, leaving "api/users"
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"path": "api/users"},
          "except": "^/v[0-9]+/"
        }],
        "responses": [{"is": {"statusCode": 200, "body": "version stripped"}}]
      }
      """
    When I send GET request to "/v1/api/users" on imposter 4545
    Then both services should return status 200
    And both responses should have body "version stripped"

  Scenario: Except parameter with body matching
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "content"},
          "except": "\\s+"
        }],
        "responses": [{"is": {"statusCode": 200}}]
      }
      """
    When I send POST request with body "  content  " on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # JSONPath Predicate Parameter
  # ==========================================================================

  Scenario: JSONPath predicate matches nested JSON field
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "john"},
          "jsonpath": {"selector": "$.user.name"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "jsonpath matched"}}]
      }
      """
    When I send POST request with JSON body '{"user": {"name": "john", "email": "john@example.com"}}' on imposter 4545
    Then both services should return status 200
    And both responses should have body "jsonpath matched"

  Scenario: JSONPath predicate with array selector
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "item1"},
          "jsonpath": {"selector": "$.items[0].name"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "array item matched"}}]
      }
      """
    When I send POST request with JSON body '{"items": [{"name": "item1"}, {"name": "item2"}]}' on imposter 4545
    Then both services should return status 200

  Scenario: JSONPath predicate with wildcard
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "contains": {"body": "active"},
          "jsonpath": {"selector": "$.users[*].status"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "wildcard matched"}}]
      }
      """
    When I send POST request with JSON body '{"users": [{"status": "disabled"}, {"status": "active"}]}' on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # XPath Predicate Parameter
  # ==========================================================================

  Scenario: XPath predicate matches XML element
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "john"},
          "xpath": {"selector": "//user/name"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "xpath matched"}}]
      }
      """
    When I send POST request with body "<root><user><name>john</name></user></root>" on imposter 4545
    Then both services should return status 200
    And both responses should have body "xpath matched"

  Scenario: XPath predicate with namespace
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "test"},
          "xpath": {
            "selector": "//ns:item/ns:value",
            "ns": {"ns": "http://example.com/ns"}
          }
        }],
        "responses": [{"is": {"statusCode": 200, "body": "namespaced xpath matched"}}]
      }
      """
    When I send POST request with body "<root xmlns:ns=\"http://example.com/ns\"><ns:item><ns:value>test</ns:value></ns:item></root>" on imposter 4545
    Then both services should return status 200

  Scenario: XPath predicate with attribute
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"body": "active"},
          "xpath": {"selector": "//user/@status"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "attribute matched"}}]
      }
      """
    When I send POST request with body "<root><user status=\"active\">John</user></root>" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Inject Predicate (JavaScript)
  # ==========================================================================

  Scenario: Inject predicate with custom JavaScript logic
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "inject": "function(request) { return request.path.length > 10; }"
        }],
        "responses": [{"is": {"statusCode": 200, "body": "inject matched"}}]
      }
      """
    When I send GET request to "/very/long/path" on imposter 4545
    Then both services should return status 200
    And both responses should have body "inject matched"

  Scenario: Inject predicate accessing multiple request fields
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "inject": "function(request) { return request.method === 'POST' && request.headers['X-Custom'] === 'check'; }"
        }],
        "responses": [{"is": {"statusCode": 200, "body": "multi-field match"}}]
      }
      """
    When I send POST request to "/" with header "X-Custom: check" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Case Sensitivity Options
  # ==========================================================================

  Scenario: Case insensitive matching is default
    # Note: Mountebank defaults to case-insensitive path matching
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404},
        "stubs": [{
          "predicates": [{"equals": {"path": "/API"}}],
          "responses": [{"is": {"statusCode": 200}}]
        }]
      }
      """
    When I send GET request to "/api" on imposter 4545
    Then both services should return status 200

  Scenario: Case insensitive header matching
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "equals": {"headers": {"X-Custom": "VALUE"}},
          "caseSensitive": false
        }],
        "responses": [{"is": {"statusCode": 200, "body": "header matched"}}]
      }
      """
    When I send GET request with header "x-custom: value" on imposter 4545
    Then both services should return status 200
