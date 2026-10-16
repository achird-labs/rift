@rift
Feature: Incremental reload (rift#316) — POST /admin/reload reconciles toward --configfile

  @reload-engine
  Scenario: reload against an identical configfile reports no changes and preserves runtime state
    Given the engine is started with the reload config file
    When the client sends "GET" "/a" to port 4870
    And the client sends "GET" "/a" to port 4870
    Then imposter 4870 has recorded 2 requests
    When the engine reloads its configuration
    Then the reload report created 0 imposters
    And the reload report deleted 0 imposters
    And the reload report exposes all four change fields
    And imposter 4870 has recorded 2 requests
    When the client sends "GET" "/a" to port 4870
    Then the response body is "A"
    When the client sends "GET" "/b" to port 4871
    Then the response body is "B"

  # rift#1160 (0.18.0): POST /admin/reload swaps the config file's `routes` block into the running
  # front door; before, it validated the block and dropped it. @front-door is external-only: it needs
  # the engine started with `--front-door`, which only the spawned engine accepts.
  @front-door @reload-engine
  Scenario: reload re-applies an edited front-door routes block
    Given the engine is started with the front door and a route from host "a.test" to port 4872
    When the client sends "GET" "/x" through the front door with host "a.test"
    Then the response status is 200
    And the response body is "A"
    When the reload config file routes host "b.test" to port 4873
    And the engine reloads its configuration
    And the client sends "GET" "/x" through the front door with host "b.test"
    Then the response status is 200
    And the response body is "B"
    When the client sends "GET" "/x" through the front door with host "a.test"
    Then the response status is 404
    And the response header "x-rift-front-door" is "no-route"

  @front-door @reload-engine
  Scenario: an invalid route table refuses the reload and the old routes keep serving
    Given the engine is started with the front door and a route from host "a.test" to port 4872
    When the reload config file declares two routes with the same id
    And the engine reload is attempted
    Then the reload is refused naming "duplicate route id"
    When the client sends "GET" "/x" through the front door with host "a.test"
    Then the response status is 200
    And the response body is "A"

  @front-door @reload-engine
  Scenario: a routes block without --front-door reloads with a warning naming the flag
    Given the engine is started without the front door and a route from host "a.test" to port 4872
    When the engine reloads its configuration
    Then the reload report warns about "--front-door"

  # docs/features/hot-reload.md: a reload classifies every imposter. An added one is `created`, a
  # removed one `deleted`, an imposter-level change (here `name`) `replaced` with fresh state, and a
  # change to one stub out of four `stubPatched`, which keeps the recorded requests and the `repeat`
  # position of the unchanged stubs. The four fields are port arrays.
  @reload-engine
  Scenario: a reload reports created, deleted, replaced and stubPatched, and keeps patched state
    Given the engine is started with the reload fixture "reload/diff-before"
    When the client sends "GET" "/x" to port 4875
    And the client sends "GET" "/a" to port 4876
    And the client sends "GET" "/cycle" to port 4876
    Then the response body is "r1"
    And imposter 4875 has recorded 1 requests
    And imposter 4876 has recorded 2 requests
    When the reload config file is rewritten to fixture "reload/diff-after"
    And the engine reloads its configuration
    Then the reload report created port 4877
    And the reload report deleted port 4874
    And the reload report replaced port 4875
    And the reload report stubPatched port 4876
    And the reload report message is "Reloaded 3 imposter(s)"
    And imposter 4875 has recorded 0 requests
    And imposter 4876 has recorded 2 requests
    When the client sends "GET" "/cycle" to port 4876
    Then the response body is "r2"
    When the client sends "GET" "/b" to port 4876
    Then the response body is "B-patched"
    When the client sends "GET" "/x" to port 4877
    Then the response body is "added"
    And the imposter on port 4874 no longer serves

  # A stub's key is its explicit `id`, so reordering keyed stubs is a patch, not a replace.
  @reload-engine
  Scenario: reordering stubs with explicit ids is a stub patch that keeps the recorded requests
    Given the engine is started with the reload fixture "reload/ids-before"
    When the client sends "GET" "/a" to port 4878
    And the client sends "GET" "/b" to port 4878
    Then imposter 4878 has recorded 2 requests
    When the reload config file is rewritten to fixture "reload/ids-after"
    And the engine reloads its configuration
    Then the reload report stubPatched port 4878
    And the reload report replaced 0 imposters
    And the reload report created 0 imposters
    And the reload report deleted 0 imposters
    And imposter 4878 has recorded 2 requests

  @reload-engine
  Scenario: an unparseable config refuses the reload with a 500 and the old imposters keep serving
    Given the engine is started with the reload config file
    When the reload config file is rewritten to invalid JSON
    And the engine reload is attempted
    Then the response status is 500
    And the reload is refused naming "Reload failed (imposters unchanged)"
    When the client sends "GET" "/a" to port 4870
    Then the response body is "A"

  # The config's `intercept` block is re-seeded on reload (rift#1271); `"imposters": []` keeps the
  # scenario off the fixed reload ports.
  @reload-engine
  Scenario: a reload re-seeds the config file's intercept rules (rift#1271)
    Given the engine is started with the reload fixture "reload/intercept-before"
    When the reload config file is rewritten to fixture "reload/intercept-after"
    And the engine reloads its configuration
    Then the response JSON at "intercept.rulesSeeded" is the number 2
    And the response JSON at "intercept.rulesRuntime" is the number 0
    When the client sends admin "GET" "/intercept/rules" with payload "-"
    Then the response is a JSON array of 2 elements
    And the response JSON at "0.action.serve.body" is the string "v2"

  # The shared engine has no config source (external mode). The embedded harness always reloads
  # from a config file, so @no-config-source is excluded there.
  @no-config-source
  Scenario: a reload with no config source reports that there is nothing to reload
    When the engine reloads its configuration
    Then the reload report message is "No config source configured; nothing to reload"
