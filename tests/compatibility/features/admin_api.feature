Feature: Admin API Compatibility
  The Rift admin API should be fully compatible with Mountebank's admin API

  Background:
    Given both Mountebank and Rift services are running
    And all imposters are cleared

  # ==========================================================================
  # Root Endpoint
  # ==========================================================================

  Scenario: Root endpoint returns service information
    When I send GET request to "/" on both services
    Then both services should return status 200
    And both responses should be valid JSON

  # ==========================================================================
  # Imposter CRUD Operations
  # ==========================================================================

  Scenario: List imposters when empty
    When I send GET request to "/imposters" on both services
    Then both services should return status 200
    And both responses should have empty imposters array

  Scenario: Create a simple imposter
    When I create an imposter on both services:
      """
      {
        "port": 4545,
        "protocol": "http",
        "name": "Test Imposter"
      }
      """
    Then both services should return status 201
    And the imposter should be accessible on port 4545

  Scenario: Create imposter with stubs
    When I create an imposter on both services:
      """
      {
        "port": 4545,
        "protocol": "http",
        "stubs": [
          {
            "predicates": [{"equals": {"path": "/test"}}],
            "responses": [{"is": {"statusCode": 200, "body": "hello"}}]
          }
        ]
      }
      """
    Then both services should return status 201
    And GET "/test" on imposter 4545 should return "hello" on both

  Scenario: Get imposter by port
    Given an imposter exists on port 4545 with name "My Imposter"
    When I send GET request to "/imposters/4545" on both services
    Then both services should return status 200
    And both responses should contain imposter with name "My Imposter"

  Scenario: Get non-existent imposter returns 404
    When I send GET request to "/imposters/9999" on both services
    Then both services should return status 404

  Scenario: Delete imposter by port
    Given an imposter exists on port 4545
    When I send DELETE request to "/imposters/4545" on both services
    Then both services should return status 200
    And GET "/imposters/4545" should return 404 on both services

  Scenario: Delete non-existent imposter
    # Mountebank returns 200 for idempotent delete (even if imposter doesn't exist)
    When I send DELETE request to "/imposters/9999" on both services
    Then both services should return status 200

  # ==========================================================================
  # Batch Operations
  # ==========================================================================

  Scenario: Replace all imposters with PUT
    Given an imposter exists on port 4545
    When I PUT to "/imposters" on both services:
      """
      {
        "imposters": [
          {"port": 4546, "protocol": "http"},
          {"port": 4547, "protocol": "http"}
        ]
      }
      """
    Then both services should return status 200
    And imposter 4545 should not exist on both services
    And imposter 4546 should exist on both services
    And imposter 4547 should exist on both services

  Scenario: Delete all imposters
    Given imposters exist on ports 4545, 4546, 4547
    When I send DELETE request to "/imposters" on both services
    Then both services should return status 200
    And no imposters should exist on both services

  # ==========================================================================
  # Query Parameters
  # ==========================================================================

  Scenario: Get imposters with replayable format
    Given an imposter exists on port 4545 with stubs
    When I send GET request to "/imposters?replayable=true" on both services
    Then both services should return status 200
    And responses should be in replayable format

  Scenario: Get imposter without requests
    Given an imposter exists on port 4545 with recordRequests enabled
    And requests have been made to imposter 4545
    When I send GET request to "/imposters/4545?removeProxies=true" on both services
    Then both services should return status 200

  # ==========================================================================
  # Stub Management Endpoints
  # ==========================================================================

  Scenario: Delete stub by index
    Given an imposter on port 4545 with stubs:
      """
      [
        {"predicates": [{"equals": {"path": "/first"}}], "responses": [{"is": {"statusCode": 200, "body": "first"}}]},
        {"predicates": [{"equals": {"path": "/second"}}], "responses": [{"is": {"statusCode": 200, "body": "second"}}]}
      ]
      """
    When I send DELETE request to "/imposters/4545/stubs/0" on both services
    Then both services should return status 200
    And GET "/first" on imposter 4545 should not return "first" on both
    And GET "/second" on imposter 4545 should return "second" on both

  Scenario: Replace all stubs
    Given an imposter on port 4545 with stub:
      """
      {"predicates": [{"equals": {"path": "/old"}}], "responses": [{"is": {"statusCode": 200, "body": "old"}}]}
      """
    When I PUT to "/imposters/4545/stubs" on both services:
      """
      {
        "stubs": [
          {"predicates": [{"equals": {"path": "/new1"}}], "responses": [{"is": {"statusCode": 200, "body": "new1"}}]},
          {"predicates": [{"equals": {"path": "/new2"}}], "responses": [{"is": {"statusCode": 200, "body": "new2"}}]}
        ]
      }
      """
    Then both services should return status 200
    And GET "/old" on imposter 4545 should not return "old" on both
    And GET "/new1" on imposter 4545 should return "new1" on both
    And GET "/new2" on imposter 4545 should return "new2" on both

  # ==========================================================================
  # Server Info Endpoints
  # ==========================================================================

  Scenario: Get server config
    When I send GET request to "/config" on both services
    Then both services should return status 200
    And both responses should contain version information

  Scenario: Get server logs
    When I send GET request to "/logs" on both services
    Then both services should return status 200
    And both responses should have logs array

  Scenario: Get logs with pagination
    When I send GET request to "/logs?startIndex=0&endIndex=10" on both services
    Then both services should return status 200

  # ==========================================================================
  # Proxy Response Management
  # ==========================================================================

  Scenario: Delete saved proxy responses
    Given an imposter on port 4545 with proxy stub
    When I send DELETE request to "/imposters/4545/savedProxyResponses" on both services
    Then both services should return status 200

  # ==========================================================================
  # Error Handling
  # ==========================================================================

  Scenario: Invalid JSON returns 400
    When I send POST with invalid JSON to "/imposters" on both services
    Then both services should return status 400
    And both responses should contain error message

  # Note: Rift intentionally accepts imposters without 'protocol' field, defaulting to "http".
  # This is a deliberate design choice for convenience. Mountebank requires protocol explicitly.
  # Test changed to Mountebank-only to document this behavioral difference.
  @mountebank-only
  Scenario: Invalid imposter config returns 400 (Mountebank only)
    When I POST to "/imposters" with missing required fields on Mountebank:
      """
      {"invalid": "config"}
      """
    Then Mountebank should return status 400

  # ==========================================================================
  # Combined Query Parameters
  # ==========================================================================

  Scenario: Get imposters with both replayable and removeProxies
    Given an imposter exists on port 4545 with proxy stub
    When I send GET request to "/imposters?replayable=true&removeProxies=true" on both services
    Then both services should return status 200
    And responses should not contain proxy responses

  Scenario: Get single imposter with both query params
    Given an imposter exists on port 4545 with proxy stub
    When I send GET request to "/imposters/4545?replayable=true&removeProxies=true" on both services
    Then both services should return status 200

  # ==========================================================================
  # GET /imposters/:port/stubs - List All Stubs
  # ==========================================================================

  @rift-only
  Scenario: Get all stubs for an imposter
    Given an imposter on port 4545 with stubs:
      """
      [
        {"predicates": [{"equals": {"path": "/first"}}], "responses": [{"is": {"statusCode": 200, "body": "first"}}]},
        {"predicates": [{"equals": {"path": "/second"}}], "responses": [{"is": {"statusCode": 200, "body": "second"}}]}
      ]
      """
    When I send GET request to "/imposters/4545/stubs" on both services
    Then both services should return status 200
    And both responses should be valid JSON
    And both responses should contain "stubs"

  Scenario: Get stubs for non-existent imposter returns 404
    When I send GET request to "/imposters/9999/stubs" on both services
    Then both services should return status 404

  # ==========================================================================
  # GET /imposters/:port/stubs/:index - Get Single Stub
  # ==========================================================================

  @rift-only
  Scenario: Get stub by index
    Given an imposter on port 4545 with stubs:
      """
      [
        {"predicates": [{"equals": {"path": "/zero"}}], "responses": [{"is": {"statusCode": 200, "body": "stub zero"}}]},
        {"predicates": [{"equals": {"path": "/one"}}], "responses": [{"is": {"statusCode": 200, "body": "stub one"}}]}
      ]
      """
    When I send GET request to "/imposters/4545/stubs/0" on both services
    Then both services should return status 200
    And both responses should contain "/zero"
    When I send GET request to "/imposters/4545/stubs/1" on both services
    Then both services should return status 200
    And both responses should contain "/one"

  Scenario: Get stub with out-of-range index returns 404
    Given an imposter on port 4545 with stub:
      """
      {"predicates": [{"equals": {"path": "/test"}}], "responses": [{"is": {"statusCode": 200, "body": "test"}}]}
      """
    When I send GET request to "/imposters/4545/stubs/99" on both services
    Then both services should return status 404

  Scenario: Get stub for non-existent imposter returns 404
    When I send GET request to "/imposters/9999/stubs/0" on both services
    Then both services should return status 404

  # ==========================================================================
  # DELETE /requests alias for savedRequests
  # ==========================================================================

  Scenario: DELETE requests path is alias for savedRequests
    Given an imposter on port 4545 with recordRequests enabled
    And I send 3 GET requests to "/test" on imposter 4545
    When I send DELETE request to "/imposters/4545/requests" on both admin APIs
    Then both services should return status 200
    And imposter 4545 should have numberOfRequests equal to 0 on both services
