# Changelog

All notable changes to Rift are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This changelog was backfilled from git history and release tags; it summarizes user-facing
changes and omits internal refactors, CI, and test-only commits. See the git log for the full
record.

## [Unreleased]

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

[Unreleased]: https://github.com/EtaCassiopeia/rift/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/EtaCassiopeia/rift/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/EtaCassiopeia/rift/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/EtaCassiopeia/rift/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/EtaCassiopeia/rift/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/EtaCassiopeia/rift/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/EtaCassiopeia/rift/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/EtaCassiopeia/rift/compare/v0.1.0-RC13...v0.2.0
[0.1.0-RC13]: https://github.com/EtaCassiopeia/rift/releases/tag/v0.1.0-RC13
