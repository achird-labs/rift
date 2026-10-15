@rift @gate
Feature: Cross-tool consistency — engine, rift-lint, and rift-verify agree on the corpus

  # Rift's admin API exposes no version endpoint, so version coherence is asserted from BUILD_INFO
  # (written by scripts/build-rift.sh) plus co-location of the tool binaries with the engine build.
  @preflight
  Scenario: Version coherence — the engine, rift-lint and rift-verify are one released build
    Then BUILD_INFO records a released build and all three binaries
    And the engine, rift-lint and rift-verify binaries match BUILD_INFO

  Scenario: The corpus is present and complete
    Given the corpus directory is loaded
    Then the corpus contains at least 15 imposter files

  Scenario: rift-lint accepts every corpus file
    Then rift-lint reports 0 errors across the corpus

  # @full-corpus: posts/serves the ENTIRE corpus, including the scripting imposter
  # (05-scripting-engines.json). This now runs under RIFT_MODE=container too: the original blocker —
  # published images lacking the `lua` cargo feature the corpus used — is void (rift 0.12.0 removed
  # Lua #451; 05-scripting is now rhai + JS, both built-in), and a green RIFT_IT run against the
  # v0.13.1 image confirms the full corpus posts + answers on its declared ports in-container (#71).
  @full-corpus
  Scenario: The engine serves every corpus imposter on its declared port
    Given every corpus imposter is posted to the engine
    Then every corpus imposter answers on its declared port

  # @full-corpus-verify: rift-verify's `--verify-dynamic` oracle stays external/embedded-only — and
  # NOT because of any lua/image gap. `--verify-dynamic` makes the engine spin up its own EPHEMERAL
  # imposters on dynamically-chosen high ports; when the engine runs inside a container those bind in
  # the container's network namespace and are unreachable from the host-side rift-verify over the 1:1
  # published-port topology (DESIGN §3.1). Container mode excludes this scenario by design (#71).
  @full-corpus-verify
  Scenario: rift-verify drives the loaded corpus with no failures
    Given every corpus imposter is posted to the engine
    Then rift-verify reports 0 failures
