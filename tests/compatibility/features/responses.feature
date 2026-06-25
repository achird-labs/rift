Feature: Response Behavior Compatibility
  Rift should handle response behaviors identically to Mountebank

  Background:
    Given both Mountebank and Rift services are running
    And all imposters are cleared

  # ==========================================================================
  # Basic Response
  # ==========================================================================

  Scenario: Return configured status code
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/status"}}],
        "responses": [{"is": {"statusCode": 418}}]
      }
      """
    When I send GET request to "/status" on imposter 4545
    Then both services should return status 418

  Scenario: Return configured headers
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/headers"}}],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": {
              "X-Custom-Header": "custom-value",
              "Content-Type": "application/json"
            }
          }
        }]
      }
      """
    When I send GET request to "/headers" on imposter 4545
    Then both services should return status 200
    And both responses should have header "X-Custom-Header" with value "custom-value"
    And both responses should have header "Content-Type" with value "application/json"

  Scenario: Return string body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/string"}}],
        "responses": [{"is": {"statusCode": 200, "body": "plain text response"}}]
      }
      """
    When I send GET request to "/string" on imposter 4545
    Then both services should return status 200
    And both responses should have body "plain text response"

  Scenario: Return JSON object body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/json"}}],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": {"Content-Type": "application/json"},
            "body": {"key": "value", "nested": {"a": 1}}
          }
        }]
      }
      """
    When I send GET request to "/json" on imposter 4545
    Then both services should return status 200
    And both responses should have JSON body with key "key" equal to "value"

  # ==========================================================================
  # Response Cycling
  # ==========================================================================

  Scenario: Cycle through multiple responses
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/cycle"}}],
        "responses": [
          {"is": {"statusCode": 200, "body": "first"}},
          {"is": {"statusCode": 200, "body": "second"}},
          {"is": {"statusCode": 200, "body": "third"}}
        ]
      }
      """
    When I send 6 GET requests to "/cycle" on imposter 4545
    Then responses should cycle: "first", "second", "third", "first", "second", "third"
    And both services should return identical response sequences

  # ==========================================================================
  # Wait Behavior
  # ==========================================================================

  Scenario: Wait behavior delays response
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/slow"}}],
        "responses": [{
          "is": {"statusCode": 200, "body": "delayed"},
          "_behaviors": {"wait": 200}
        }]
      }
      """
    When I send GET request to "/slow" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should take at least 200ms

  Scenario: Wait behavior with function (random delay)
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/random-wait"}}],
        "responses": [{
          "is": {"statusCode": 200},
          "_behaviors": {"wait": "function() { return Math.floor(Math.random() * 100) + 50; }"}
        }]
      }
      """
    When I send GET request to "/random-wait" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should take at least 50ms

  # ==========================================================================
  # Repeat Behavior
  # ==========================================================================

  Scenario: Repeat behavior repeats response before cycling
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/repeat"}}],
        "responses": [
          {
            "is": {"statusCode": 200, "body": "repeated"},
            "_behaviors": {"repeat": 3}
          },
          {"is": {"statusCode": 200, "body": "after"}}
        ]
      }
      """
    When I send 5 GET requests to "/repeat" on imposter 4545
    Then responses should be: "repeated", "repeated", "repeated", "after", "repeated"
    And both services should return identical response sequences

  # ==========================================================================
  # Decorate Behavior
  # ==========================================================================

  Scenario: Decorate behavior modifies response
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/decorate"}}],
        "responses": [{
          "is": {"statusCode": 200, "body": "original"},
          "_behaviors": {
            "decorate": "function(request, response) { response.body = response.body + ' decorated'; }"
          }
        }]
      }
      """
    When I send GET request to "/decorate" on imposter 4545
    Then both services should return status 200
    And both responses should have body "original decorated"

  # ==========================================================================
  # Copy Behavior
  # ==========================================================================

  Scenario: Copy behavior copies from request
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": {"X-Request-Id": "${REQUEST_ID}"},
            "body": "copied"
          },
          "_behaviors": {
            "copy": {
              "from": {"headers": "X-Request-Id"},
              "into": "${REQUEST_ID}",
              "using": {"method": "regex", "selector": ".*"}
            }
          }
        }]
      }
      """
    When I send GET request with header "X-Request-Id: abc123" on imposter 4545
    Then both services should return status 200
    And both responses should have header "X-Request-Id" with value "abc123"

  # ==========================================================================
  # Default Response
  # ==========================================================================

  Scenario: Default response when no stub matches
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {
          "statusCode": 503,
          "body": "Service Unavailable"
        },
        "stubs": []
      }
      """
    When I send GET request to "/unmatched" on imposter 4545
    Then both services should return status 503
    And both responses should have body "Service Unavailable"

  # ==========================================================================
  # Fault Injection
  # ==========================================================================

  Scenario: Fault injection returns error
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/fault"}}],
        "responses": [{
          "fault": "CONNECTION_RESET_BY_PEER"
        }]
      }
      """
    When I send GET request to "/fault" on imposter 4545
    Then both services should return connection error

  Scenario: RANDOM_DATA_THEN_CLOSE fault sends garbage and closes
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/random-fault"}}],
        "responses": [{
          "fault": "RANDOM_DATA_THEN_CLOSE"
        }]
      }
      """
    When I send GET request to "/random-fault" on imposter 4545
    Then both services should return connection error or invalid response

  # ==========================================================================
  # Inject Response (JavaScript)
  # ==========================================================================

  Scenario: Inject response generates dynamic response
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "inject": "function(request) { return { statusCode: 200, body: 'Path: ' + request.path }; }"
        }]
      }
      """
    When I send GET request to "/dynamic" on imposter 4545
    Then both services should return status 200
    And both responses should have body "Path: /dynamic"

  Scenario: Inject response with state
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "inject": "function(request, state) { state.count = (state.count || 0) + 1; return { statusCode: 200, body: 'Count: ' + state.count }; }"
        }]
      }
      """
    When I send GET request to "/count" on imposter 4545
    Then both responses should have body "Count: 1"
    When I send GET request to "/count" on imposter 4545
    Then both responses should have body "Count: 2"

  Scenario: Inject response with async callback
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "inject": "function(request, state, logger, callback) { callback({ statusCode: 201, body: 'async response' }); }"
        }]
      }
      """
    When I send GET request to "/async" on imposter 4545
    Then both services should return status 201
    And both responses should have body "async response"

  # ==========================================================================
  # ShellTransform Behavior
  # Note: shellTransform requires shell execution, which Rift does not support
  # ==========================================================================

  @skip @rift-unsupported
  Scenario: ShellTransform behavior modifies response via shell command
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {"statusCode": 200, "body": "original"},
          "_behaviors": {
            "shellTransform": "printf '{\"body\": \"transformed\"}'"
          }
        }]
      }
      """
    When I send GET request to "/shell" on imposter 4545
    Then both services should return status 200
    And both responses should have body "transformed"

  # ==========================================================================
  # Copy Behavior Advanced
  # ==========================================================================

  Scenario: Copy behavior with JSONPath extraction
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": "User: ${USER_NAME}"
          },
          "_behaviors": {
            "copy": {
              "from": "body",
              "into": "${USER_NAME}",
              "using": {"method": "jsonpath", "selector": "$.user.name"}
            }
          }
        }]
      }
      """
    When I send POST request with JSON body '{"user": {"name": "Alice"}}' on imposter 4545
    Then both services should return status 200
    And both responses should have body "User: Alice"

  Scenario: Copy behavior with XPath extraction
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": "Name: ${NAME}"
          },
          "_behaviors": {
            "copy": {
              "from": "body",
              "into": "${NAME}",
              "using": {"method": "xpath", "selector": "//user/name/text()"}
            }
          }
        }]
      }
      """
    When I send POST request with body "<root><user><name>Bob</name></user></root>" on imposter 4545
    Then both services should return status 200
    And both responses should have body "Name: Bob"

  Scenario: Copy behavior from query parameters
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": "Search: ${QUERY}"
          },
          "_behaviors": {
            "copy": {
              "from": {"query": "q"},
              "into": "${QUERY}",
              "using": {"method": "regex", "selector": ".*"}
            }
          }
        }]
      }
      """
    When I send GET request to "/?q=hello" on imposter 4545
    Then both services should return status 200
    And both responses should have body "Search: hello"

  Scenario: Multiple copy behaviors
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "headers": {"X-Request-Id": "${ID}"},
            "body": "Method: ${METHOD}"
          },
          "_behaviors": {
            "copy": [
              {"from": {"headers": "X-Request-Id"}, "into": "${ID}", "using": {"method": "regex", "selector": ".*"}},
              {"from": "method", "into": "${METHOD}", "using": {"method": "regex", "selector": ".*"}}
            ]
          }
        }]
      }
      """
    When I send POST request with header "X-Request-Id: req-123" on imposter 4545
    Then both services should return status 200
    And both responses should have body "Method: POST"
    And both responses should have header "X-Request-Id" with value "req-123"

  # ==========================================================================
  # Decorate Behavior Advanced
  # ==========================================================================

  Scenario: Decorate behavior adds custom headers
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {"statusCode": 200, "body": "test"},
          "_behaviors": {
            "decorate": "function(request, response) { response.headers = response.headers || {}; response.headers['X-Request-Path'] = request.path; }"
          }
        }]
      }
      """
    When I send GET request to "/decorated" on imposter 4545
    Then both services should return status 200
    And both responses should have header "X-Request-Path" with value "/decorated"

  Scenario: Decorate behavior modifies status code conditionally
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {"statusCode": 200, "body": "test"},
          "_behaviors": {
            "decorate": "function(request, response) { var h = request.headers['x-force-error'] || request.headers['X-Force-Error']; if (h) { response.statusCode = 500; response.body = 'Forced error'; } }"
          }
        }]
      }
      """
    When I send GET request with header "X-Force-Error: true" on imposter 4545
    Then both services should return status 500
    And both responses should have body "Forced error"

  # ==========================================================================
  # Wait Behavior Advanced
  # ==========================================================================

  Scenario: Wait behavior with JavaScript function returning random delay
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {"statusCode": 200, "body": "waited"},
          "_behaviors": {
            "wait": "function() { return Math.floor(Math.random() * 50) + 100; }"
          }
        }]
      }
      """
    When I send GET request to "/" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should take at least 100ms

  # ==========================================================================
  # Lookup Behavior
  # ==========================================================================

  # Note: Lookup behavior requires a CSV file, which may need setup
  # This test verifies the basic structure is accepted

  Scenario: Lookup behavior basic structure is accepted
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": "User: ${row}['name']"
          },
          "_behaviors": {
            "lookup": {
              "key": {"from": {"query": "id"}, "using": {"method": "regex", "selector": ".*"}},
              "fromDataSource": {
                "csv": {"path": "/data/users.csv", "keyColumn": "id"}
              },
              "into": "${row}"
            }
          }
        }]
      }
      """
    # Note: This may return 200 with unsubstituted template if CSV doesn't exist
    # The test verifies the configuration is accepted
    When I send GET request to "/?id=1" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Multiple Behaviors Chain
  # ==========================================================================

  Scenario: Multiple behaviors execute in order
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [],
        "responses": [{
          "is": {"statusCode": 200, "body": "start"},
          "_behaviors": {
            "wait": 50,
            "decorate": "function(request, response) { response.body = response.body + '-decorated'; }",
            "repeat": 2
          }
        }]
      }
      """
    When I send GET request to "/" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should have body "start-decorated"
    And both responses should take at least 50ms

  # ==========================================================================
  # ShellTransform Array Format Accepted (format, not execution)
  # ==========================================================================

  @rift-only
  Scenario: shellTransform array format is accepted without parse error
    Given an imposter on port 4545 on Rift with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "predicates": [{"equals": {"path": "/shell-array"}}],
          "responses": [{
            "is": {"statusCode": 200, "body": "original"},
            "_behaviors": {
              "shellTransform": ["./transform1.sh", "./transform2.sh"]
            }
          }]
        }]
      }
      """
    When I send GET request to "/shell-array" on Rift imposter 4545
    Then Rift should return status 200
    And Rift response body should be "original"
