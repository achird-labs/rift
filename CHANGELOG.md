# Changelog

All notable changes to Rift are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This changelog was backfilled from git history and release tags; it summarizes user-facing
changes and omits internal refactors, CI, and test-only commits. See the git log for the full
record.

## [Unreleased]

## [0.11.2] - 2026-07-08

### Fixed
- **`rift_start_intercept` now returns a truthful `interceptUrl`** (#425). It previously hardcoded
  `http://127.0.0.1:<port>` even when the listener bound a non-loopback host, misreporting the
  endpoint as loopback for a `0.0.0.0`/NIC-IP bind (e.g. for cross-container reachability). The URL
  and port are now derived from the listener's real bound address; the loopback default is unchanged.

### Changed
- **Refreshed the Rift vs Mountebank benchmark and README performance numbers** (#419). Added a
  no-Docker, direct-process harness (`tests/benchmark/scripts/bench_direct.py`) that runs each engine
  in isolation on disjoint ports and asserts every scenario's response body actually matches before
  measuring, replacing the stale figures with reproducible ones.

## [0.11.1] - 2026-07-07

### Added
- **`rift_flow_state_get` now gives an unambiguous "not found" signal** over the embedded C-ABI
  (#416). It previously returned `null` both for an absent key and for an error, so a host could
  not tell the two apart; the call now reports "not found" distinctly from a genuine failure.

### Fixed
- **PKCS#12 truststore export is now loadable as a JVM trust store** (#417). `export_pkcs12` (and
  thus `rift_intercept_export_truststore` / `GET /intercept/truststore.p12`) previously wrote the CA
  as a bare cert bag that a JVM `TrustManagerFactory` did not surface as a trust anchor — TLS
  validation failed with "the trustAnchors parameter must be non-empty". The export now carries the
  trusted-certificate marker `keytool` writes, so the CA loads as a trust anchor, matching the JKS
  export.

## [0.11.0] - 2026-07-07

### Added
- **Direct C-ABI over `librift_ffi` for the embedded control plane**, so a host process (e.g. a JVM
  test harness) can drive Rift without standing up the loopback admin-HTTP server. `rift_serve_admin`
  becomes optional.
  - Scenario state & correlated spaces (#412): `rift_flow_state_get` / `rift_flow_state_put` /
    `rift_flow_state_delete` and `rift_space_add_stub` / `rift_space_list_stubs` /
    `rift_space_delete` / `rift_space_recorded` wrap the same `ImposterManager`/`Imposter` methods
    the admin-HTTP handlers use.
  - Intercept listener & rules (#413): `rift_start_intercept`, `rift_intercept_add_rules`,
    `rift_intercept_list_rules`, `rift_intercept_clear_rules`, `rift_intercept_ca_pem`,
    `rift_intercept_export_truststore` (writes a JVM-consumable trustStore to a file path), and
    `rift_stop` for teardown — the FFI equivalent of the intercept admin API shipped in 0.10.0.
  - Regenerated cbindgen header; documented in `docs/embedding/ffi.md`.

## [0.10.0] - 2026-07-07

### Added
- **Built-in intercept/redirect proxy mode (TLS-MITM).** Rift can now sit in the request path as an opt-in HTTPS forward proxy to mock an external dependency whose host the system-under-test hard-codes (e.g. a feature-flag SDK that always fetches `https://cdn.example.com/config.json`) — replacing an external mitmproxy sidecar with no committed crypto. It accepts HTTP `CONNECT`, TLS-terminates using an intercept CA (generated at startup, or loaded from PEM) that mints a per-SNI leaf certificate on demand, matches the decrypted request with the existing predicate engine, and either serves an inline stub or forwards it to an imposter port.
  - Admin API: `POST`/`GET`/`DELETE /intercept/rules`, plus `GET /intercept/ca.pem` and `/intercept/truststore.p12` / `/intercept/truststore.jks` to export the CA cert and a ready-to-use PKCS#12 or JKS truststore (with password) for a SUT to trust.
  - Standalone binary: `--intercept-port` (env `RIFT_INTERCEPT_PORT`) starts the listener; `--intercept-ca-cert` / `--intercept-ca-key` load an existing CA, otherwise one is generated in-memory. The rule store and CA are shared with the admin API.
  - Documented in `docs/features/intercept-proxy.md` with a runnable end-to-end example. Non-goals: HTTP/2, WebSocket proxying, chunked request bodies, and transparent (non-`CONNECT`) interception.

## [0.9.1] - 2026-07-04

### Fixed
- Release artifacts now include the `rift-verify` binary — the platform tarballs, the Windows zip, and the Homebrew formula ship it alongside `rift`, `rift-lint`, and `rift-tui`. It was built by the release job but never packaged, so no prior release contained it.

## [0.9.0] - 2026-07-04

### Added
- HTTP/2 and h2c support via hyper auto-negotiation on HTTP and HTTPS listeners; `RIFT_DISABLE_HTTP2` escape hatch forces HTTP/1-only.
- Socket tuning: `TCP_NODELAY` is on by default, with `RIFT_TCP_NODELAY` and `RIFT_TCP_BACKLOG` knobs.
- Feature-gated `mimalloc` global allocator (default-on for `rift-http-proxy`).
- `rift-verify -o json` machine-readable summary; ANSI/banner suppression when stdout is piped or `NO_COLOR` is set.
- Opt-in strict behaviors: per-imposter `strictBehaviors` flag / `RIFT_STRICT_BEHAVIORS` env var returns `500` on a `decorate`/`shellTransform`/binary failure instead of the lenient fallback.
- Opt-in `RIFT_STRICT_FLOW_STORE` env var raises on script flow-store op failures in all three engines.
- `flow_store.last_error()` lets scripts distinguish a down backend from an empty result.
- Script wall-clock timeout (`_rift.scriptEngine.timeoutMs`, default 5000 ms) plus JS loop-iteration and recursion bounds.

### Changed
- `POST /admin/reload` is now incremental (diff-based): unchanged imposters and stubs keep their runtime state, and the response reports `created`/`replaced`/`stubPatched`/`deleted`.
- Top-level `fault` responses reset the connection at the transport level (Mountebank parity) instead of returning HTTP 502.
- `decorate`/`shellTransform`/binary behavior failures are signaled via `x-rift-<behavior>-error` response headers instead of being served silently.
- Bare `jsonpath` selectors (no leading `$`) are treated as root-relative.
- Flat stub responses (top-level `statusCode`/`headers`/`body`, no `is` wrapper) are accepted and served identically to the wrapped form.

### Fixed
- An unknown `flowState` backend, or an unreachable/misconfigured redis flow-store, now fails imposter creation with `400` instead of silently downgrading to a no-op store.
- Runaway `_rift.script` execution is bounded so it can no longer wedge the engine.

## [0.8.0] - 2026-07-01

### Added
- Past offsets in date templates: `{{DAYS-N}}` / `{{MONTHS-N}}` (complementing `{{DAYS+N}}` / `{{MONTHS+N}}` / `{{NOW}}`).
- `rift-verify --verify-dynamic` asserts dynamic behaviors (inject/proxy/script, faults, binary mode, inline-flag regex, exists-query).

### Changed
- Flow-state config: `flowIdSource` is now flattened under `_rift.flowState`; the `mountebankStateMapping` wrapper was dropped.

### Fixed
- TCP faults now take precedence over error faults in `_rift.fault`.
- Multi-value response headers are preserved under `copy` / `lookup` / `decorate` behaviors.
- Request templates, `shellTransform`, and metrics are applied on the static-response path.
- `rift-lint` no longer emits W009 on Rhai or Mountebank config-arrow `decorate` scripts, and accepts multi-value header string arrays (E018).

## [0.7.0] - 2026-06-29

### Added
- Per-imposter HTTPS: imposters declared with `protocol: https` terminate TLS.
- Single-port imposter access via the `/__rift/:port/<path>` gateway.
- Date templates in response bodies: `{{DAYS+N}}`, `{{MONTHS+N}}`, `{{NOW}}`.

### Fixed
- `decorate` behavior supports the JavaScript `config =>` calling convention.

## [0.6.0] - 2026-06-29

### Added
- Real connection-level TCP faults for `_rift.fault.tcp`.
- Multi-value header support for responses and recorded requests.
- Hot-reload of imposters via `POST /admin/reload`.

## [0.5.0] - 2026-06-29

### Added
- Id-addressed stub operations: add / replace / delete a stub by its `Stub.id` (`/imposters/:port/stubs/by-id/:id`).
- `rift-ffi` crate: a C-ABI over `ImposterManager` for embedding Rift in-process.

### Changed
- Engine extracted into the CLI-free `rift-core` crate; shared types moved to `rift-types`.

## [0.4.0] - 2026-06-29

### Added
- Declarative stateful scenarios (`requiredScenarioState` / `newScenarioState`) plus flow-state admin inspection endpoints.
- First-class correlated isolation ("spaces"): space-scoped stubs and per-space teardown.
- `defaultForward` fallback proxy for unmatched requests.

## [0.3.0] - 2026-06-28

### Added
- Filter recorded requests by header or flow id (`GET /imposters/:port/savedRequests?match=...`).

### Fixed
- Script request headers are exposed with lowercase keys.
- `lookup` behavior applies on direct imposter responses.
- `rift-lint` accepts the imposters-wrapper and bare-array config shapes.

## [0.2.0] - 2026-06-28

### Added
- `--apikey` flag for admin API authentication.
- `--datadir` write-through persistence for live imposter mutations.
- `--rcfile` defaults merging and `save` defaults.
- EJS token preprocessing in `--configfile` (`<% include %>`, `<%= process.env.X %>`).
- `inject` predicate operation and `predicateGenerators` support in proxy recording.
- `?list=true` query parameter on `GET /imposters`.
- `--log` file logging (via tracing-appender); accepts `--noParse`, `--formatter`, `--protofile` for Mountebank compatibility.

### Fixed
- `allowCORS` injects CORS headers and handles `OPTIONS` preflight.
- Keep-alive connections are closed on imposter delete.
- XPath namespace maps (`ns`) are applied during predicate evaluation.
- Proxy pipeline preserves multi-valued headers and binary response bodies; recording race conditions and memory-exhaustion risks fixed.
- Numerous predicate-matching correctness fixes (`exists`, `except`, `deepEquals`, multi-valued query params, regex flags).

## [0.1.0-RC1 .. 0.1.0-RC13] - 2025-11-28 .. 2026-01-04

Initial release-candidate series establishing the Mountebank-compatible core: imposters, stubs,
predicates, responses, behaviors, proxy/record, and the `_rift` extension namespace (fault
injection, multi-engine scripting, flow state).

[Unreleased]: https://github.com/EtaCassiopeia/rift/compare/v0.11.2...HEAD
[0.11.2]: https://github.com/EtaCassiopeia/rift/compare/v0.11.1...v0.11.2
[0.11.1]: https://github.com/EtaCassiopeia/rift/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/EtaCassiopeia/rift/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/EtaCassiopeia/rift/compare/v0.9.1...v0.10.0
[0.9.1]: https://github.com/EtaCassiopeia/rift/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/EtaCassiopeia/rift/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/EtaCassiopeia/rift/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/EtaCassiopeia/rift/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/EtaCassiopeia/rift/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/EtaCassiopeia/rift/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/EtaCassiopeia/rift/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/EtaCassiopeia/rift/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/EtaCassiopeia/rift/compare/v0.1.0-RC13...v0.2.0
[0.1.0-RC13]: https://github.com/EtaCassiopeia/rift/releases/tag/v0.1.0-RC13
