Feature: Alternative Mountebank Format Compatibility
  Rift should accept alternative Mountebank JSON formats that some tools generate

  Background:
    Given both Mountebank and Rift services are running
    And all imposters are cleared

  # ==========================================================================
  # StatusCode as String
  # ==========================================================================

  Scenario: StatusCode as string in response
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/string-status"}}],
        "responses": [{
          "is": {
            "statusCode": "200",
            "body": "status code was string"
          }
        }]
      }
      """
    When I send GET request to "/string-status" on imposter 4545
    Then both services should return status 200
    And both responses should have body "status code was string"

  Scenario: StatusCode as string with non-200 status
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/string-404"}}],
        "responses": [{
          "is": {
            "statusCode": "404",
            "body": "not found"
          }
        }]
      }
      """
    When I send GET request to "/string-404" on imposter 4545
    Then both services should return status 404

  # ==========================================================================
  # Behaviors With Underscore Prefix (Standard Mountebank Format)
  # ==========================================================================

  Scenario: Behaviors field with underscore prefix
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/with-underscore"}}],
        "responses": [{
          "_behaviors": {"wait": 100},
          "is": {
            "statusCode": 200,
            "body": "waited"
          }
        }]
      }
      """
    When I send GET request to "/with-underscore" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should take at least 100ms

  # ==========================================================================
  # Behaviors as Object Format (Standard Mountebank)
  # ==========================================================================

  Scenario: Behaviors as object with single behavior
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/object-behavior"}}],
        "responses": [{
          "_behaviors": {"wait": 50},
          "is": {
            "statusCode": 200,
            "body": "object format"
          }
        }]
      }
      """
    When I send GET request to "/object-behavior" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should take at least 50ms

  Scenario: Behaviors as object with multiple behaviors
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/multi-object-behavior"}}],
        "responses": [{
          "_behaviors": {
            "wait": 50,
            "decorate": "function(request, response) { response.body = response.body + ' decorated'; }"
          },
          "is": {
            "statusCode": 200,
            "body": "original"
          }
        }]
      }
      """
    When I send GET request to "/multi-object-behavior" on imposter 4545 and measure time
    Then both services should return status 200
    And both responses should have body "original decorated"

  # ==========================================================================
  # Proxy Null Alongside Is Response
  # ==========================================================================

  Scenario: Proxy null alongside is response
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/proxy-null"}}],
        "responses": [{
          "is": {
            "statusCode": 200,
            "body": "is response with proxy null"
          },
          "proxy": null
        }]
      }
      """
    When I send GET request to "/proxy-null" on imposter 4545
    Then both services should return status 200
    And both responses should have body "is response with proxy null"

  Scenario: Full alternative format with behaviors array and proxy null
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/full-alt"}}],
        "responses": [{
          "behaviors": [{"wait": 0}],
          "is": {
            "statusCode": "201",
            "headers": {"Content-Type": "application/json"},
            "body": "{\"created\": true}"
          },
          "proxy": null
        }]
      }
      """
    When I send GET request to "/full-alt" on imposter 4545
    Then both services should return status 201
    And both responses should have header "Content-Type" with value "application/json"

  # ==========================================================================
  # ScenarioName Field in Stubs
  # ==========================================================================

  Scenario: ScenarioName field in stub is accepted
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "scenarioName": "TestScenario-001",
          "predicates": [{"equals": {"path": "/scenario"}}],
          "responses": [{"is": {"statusCode": 200, "body": "scenario test"}}]
        }]
      }
      """
    When I send GET request to "/scenario" on imposter 4545
    Then both services should return status 200
    And both responses should have body "scenario test"

  # ==========================================================================
  # AllowCORS Field in Imposter Config
  # ==========================================================================

  Scenario: AllowCORS field in imposter config is accepted
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "allowCORS": true,
        "stubs": [{
          "predicates": [{"equals": {"path": "/cors"}}],
          "responses": [{"is": {"statusCode": 200, "body": "cors enabled"}}]
        }]
      }
      """
    When I send GET request to "/cors" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Service Name and Service Info Fields
  # ==========================================================================

  Scenario: Service name and info fields are accepted
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "service_name": "LenderDetails_v1_lenders",
        "service_info": {
          "virtualServiceInfo": {
            "serviceName": "LenderDetails-v1-lenders",
            "realEndpoint": "https://api.example.com/lenders"
          }
        },
        "stubs": [{
          "predicates": [{"equals": {"path": "/service-info"}}],
          "responses": [{"is": {"statusCode": 200, "body": "service info test"}}]
        }]
      }
      """
    When I send GET request to "/service-info" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # Complex Real-World Format (from user's JSON)
  # ==========================================================================

  Scenario: Complex real-world format with all alternative features
    Given an imposter on port 4545 with:
      """
      {
        "allowCORS": true,
        "protocol": "http",
        "port": 4545,
        "stubs": [{
          "scenarioName": "LenderDetails-v1-lenders_test",
          "predicates": [
            {"equals": {"query": {"lenderIds": "TEST123"}}},
            {"deepEquals": {"method": "GET"}}
          ],
          "responses": [{
            "behaviors": [{"wait": " function() { var min = Math.ceil(0); var max = Math.floor(0); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; } "}],
            "is": {
              "statusCode": "200",
              "headers": {
                "Accept": "application/json",
                "Content-Type": "application/json"
              },
              "body": "{\"lenders\": [{\"lenderId\": \"TEST123\"}]}"
            },
            "proxy": null
          }]
        }],
        "service_name": "LenderDetails_v1_lenders"
      }
      """
    When I send GET request to "/?lenderIds=TEST123" on imposter 4545
    Then both services should return status 200
    And both responses should have header "Content-Type" with value "application/json"

  # ==========================================================================
  # Contains Predicate with Query Parameters
  # ==========================================================================

  Scenario: Contains predicate matches substring in query parameter value
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "contains": {"query": {"lenderIds": "CofTest"}}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "contains matched"}}]
      }
      """
    When I send GET request to "/?lenderIds=CofTestWL" on imposter 4545
    Then both services should return status 200
    And both responses should have body "contains matched"

  Scenario: Contains predicate matches partial query value
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "contains": {"query": {"filter": "active"}}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "filter matched"}}]
      }
      """
    When I send GET request to "/?filter=is_active_user" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # DeepEquals for Method and Body
  # ==========================================================================

  Scenario: DeepEquals predicate matches method exactly
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"deepEquals": {"method": "GET"}}],
        "responses": [{"is": {"statusCode": 200, "body": "method matched"}}]
      }
      """
    When I send GET request to "/" on imposter 4545
    Then both services should return status 200
    And both responses should have body "method matched"

  Scenario: DeepEquals predicate matches empty body
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"deepEquals": {"body": ""}}],
        "responses": [{"is": {"statusCode": 200, "body": "empty body matched"}}]
      }
      """
    When I send GET request to "/" on imposter 4545
    Then both services should return status 200
    And both responses should have body "empty body matched"

  Scenario: DeepEquals predicate matches path exactly
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{
          "deepEquals": {"path": "/kaizen/auto/financing/lender-information/lenders"}
        }],
        "responses": [{"is": {"statusCode": 200, "body": "path matched"}}]
      }
      """
    When I send GET request to "/kaizen/auto/financing/lender-information/lenders" on imposter 4545
    Then both services should return status 200

  # ==========================================================================
  # EndsWith Predicate for Path
  # ==========================================================================

  Scenario: EndsWith predicate matches path suffix
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"endsWith": {"path": "lender-details"}}],
        "responses": [{"is": {"statusCode": 200, "body": "endswith matched"}}]
      }
      """
    When I send GET request to "/api/financing/lender-details" on imposter 4545
    Then both services should return status 200
    And both responses should have body "endswith matched"

  # ==========================================================================
  # Multiple Predicate Types Combined (from user's real JSON)
  # ==========================================================================

  Scenario: Multiple predicate types combined as implicit AND
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [
          {"endsWith": {"path": "lender-details"}},
          {"contains": {"query": {"lenderIds": "ALL"}}},
          {"deepEquals": {"method": "GET"}}
        ],
        "responses": [{"is": {"statusCode": 200, "body": "all predicates matched"}}]
      }
      """
    When I send GET request to "/api/lender-details?lenderIds=ALL" on imposter 4545
    Then both services should return status 200
    And both responses should have body "all predicates matched"

  Scenario: Multiple predicates fail when one doesn't match
    Given an imposter on port 4545 with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "defaultResponse": {"statusCode": 404},
        "stubs": [{
          "predicates": [
            {"endsWith": {"path": "lender-details"}},
            {"contains": {"query": {"lenderIds": "ALL"}}},
            {"deepEquals": {"method": "GET"}}
          ],
          "responses": [{"is": {"statusCode": 200}}]
        }]
      }
      """
    When I send POST request to "/api/lender-details?lenderIds=ALL" on imposter 4545
    Then both services should return status 404

  # ==========================================================================
  # Wait Behavior with JavaScript Function String
  # ==========================================================================

  Scenario: Wait behavior with JavaScript function returning zero
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/js-wait"}}],
        "responses": [{
          "behaviors": [{
            "wait": " function() { var min = Math.ceil(0); var max = Math.floor(0); var num = Math.floor(Math.random() * (max - min + 1)); var wait = (num + min); return wait; } "
          }],
          "is": {
            "statusCode": 200,
            "body": "js wait completed"
          },
          "proxy": null
        }]
      }
      """
    When I send GET request to "/js-wait" on imposter 4545
    Then both services should return status 200
    And both responses should have body "js wait completed"

  # ==========================================================================
  # Proxy Response with Redirect Mode
  # ==========================================================================

  Scenario: Proxy response with proxyTransparent mode is accepted
    Given an imposter on port 4545 with stub:
      """
      {
        "predicates": [{"equals": {"path": "/proxy-redirect"}}],
        "responses": [{
          "proxy": {
            "to": "http://localhost:9999",
            "mode": "proxyTransparent"
          }
        }]
      }
      """
    # Note: This will fail to connect but validates the format is accepted
    When I send GET request to "/proxy-redirect" on imposter 4545
    Then both services should return same error type

  # ==========================================================================
  # Legacy recorder compatibility: rules alias, delayRange, recordedFrom
  # These formats are emitted by a legacy recorder and are not part of
  # Mountebank's own format — tests run against Rift only.
  # ==========================================================================

  @rift-only
  Scenario: rules key is accepted as alias for predicates (legacy recorder format)
    Given an imposter on port 4545 on Rift with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "rules": [{"equals": {"path": "/rules-alias"}}],
          "responses": [{"is": {"statusCode": 200, "body": "rules alias matched"}}]
        }]
      }
      """
    When I send GET request to "/rules-alias" on Rift imposter 4545
    Then Rift should return status 200
    And Rift response body should be "rules alias matched"

  @rift-only
  Scenario: predicates wins over rules when both present
    Given an imposter on port 4545 on Rift with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "predicates": [{"equals": {"path": "/pred-wins"}}],
          "rules": [{"equals": {"path": "/rules-loses"}}],
          "responses": [{"is": {"statusCode": 200, "body": "predicates won"}}]
        }]
      }
      """
    When I send GET request to "/pred-wins" on Rift imposter 4545
    Then Rift should return status 200
    When I send GET request to "/rules-loses" on Rift imposter 4545
    Then Rift should return status 404

  @rift-only
  Scenario: delayRange array is converted to wait behavior (legacy recorder format)
    Given an imposter on port 4545 on Rift with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "predicates": [{"equals": {"path": "/delay-range"}}],
          "delayRange": [{"min": "100", "max": "200"}],
          "responses": [{"is": {"statusCode": 200, "body": "delayed"}}]
        }]
      }
      """
    When I send GET request to "/delay-range" on Rift imposter 4545 and measure time
    Then Rift should return status 200
    And Rift response should take at least 100ms

  @rift-only
  Scenario: recordedFrom field is preserved on stubs
    Given an imposter on port 4545 on Rift with:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [{
          "predicates": [{"equals": {"path": "/recorded"}}],
          "recordedFrom": "http://upstream:9090",
          "responses": [{"is": {"statusCode": 200, "body": "recorded stub"}}]
        }]
      }
      """
    When I send GET request to "/recorded" on Rift imposter 4545
    Then Rift should return status 200
    And Rift response body should be "recorded stub"
    When I query "/imposters/4545" on Rift admin API
    Then Rift should return status 200
    And Rift response body should contain "recordedFrom"
    And Rift response body should contain "http://upstream:9090"
