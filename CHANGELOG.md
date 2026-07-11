# Changelog

All notable changes to Rift are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This changelog was backfilled from git history and release tags; it summarizes user-facing
changes and omits internal refactors, CI, and test-only commits. See the git log for the full
record.

## [Unreleased]

### Added

- **Probabilistic TCP faults** (`_rift.fault.tcp`): the fault now accepts an object form
  `{ "probability": 0.1, "type": "CONNECTION_RESET_BY_PEER" }` alongside the existing bare string,
  so a connection reset can be made to fire only some of the time (e.g. "reset 10% of requests").
  The bare string form is unchanged and equivalent to `probability: 1.0`; each form round-trips
  unchanged through `GET /imposters`. `rift-lint` validates the object form (`probability` required
  and in `[0, 1]`, unknown fault types warned).

### Fixed

- **`--datadir` no longer silently drops malformed imposter files.** A file that can't be loaded
  (unreadable, invalid JSON, unresolvable scripts, or a creation failure) is now collected and
  reported together in a single prominent startup error listing every skipped file and why, instead
  of a per-file warning that was easy to miss — so a typo'd fixture that vanishes from the running
  set is visible. One bad file also no longer aborts loading of the remaining valid imposters.

## [0.13.1] - 2026-07-11

### Changed

- Renamed the `rift-core` crate to **`rift-mock-core`** to resolve a crates.io name collision — the
  `rift-core` name is owned by an unrelated project, which blocked publishing and had also frozen
  `rift-http-proxy` on crates.io at 0.4.0. This is purely a packaging change: the binary, FFI
  cdylib, Docker image, and npm package are functionally identical to 0.13.0. Rust code depending on
  the engine library from crates.io should switch the dependency name to `rift-mock-core`; the
  FFI / Docker / binary / npm distribution (what the language SDKs consume) is unaffected.

## [0.13.0] - 2026-07-10

This release lands the engine-side surface the official language SDKs build on — server-side
verification, the C-ABI admin long tail, a published conformance corpus, and runtime intercept —
plus a broad round of hot-path performance work and scripting/proxy fixes.

### Added
- **Server-side verification endpoint** (#494): `POST /imposters/{port}/verify` counts (and
  optionally returns) recorded requests matching a predicate set, evaluated by the engine's own
  predicate engine, and can return the closest non-match with per-clause `failedPredicates` for a
  readable diff. `flowId` scopes the count like `savedRequests`. A matching FFI symbol `rift_verify`
  gives embedded parity. This lets every SDK's `verify(match, times(n))` defer to the one true
  evaluator instead of re-implementing predicate matching (or shipping the whole journal over the
  wire), including operators impractical client-side (`xpath`, `inject`).
- **C-ABI admin long-tail symbols** (#491): twelve additive v2 FFI symbols — `rift_list_imposters`,
  `rift_get_imposter`, `rift_add_stub`, `rift_get_stub`, `rift_update_stub`, `rift_delete_stub`,
  `rift_clear_recorded`, `rift_clear_proxy_recordings`, `rift_set_imposter_enabled`,
  `rift_scenarios`, `rift_set_scenario_state`, `rift_reset_scenarios` — so an embedded SDK can drive
  the full admin surface without lazily booting an in-process admin plane over loopback HTTP. Each
  delegates to the same manager/imposter method its admin route uses.
- **`allowInjection` option for `rift_serve_admin`** (#492): the embedded admin plane now gates
  script/inject imposters submitted through it behind an explicit `allowInjection` flag (default
  off), mirroring the `--allowInjection` CLI gate.
- **Runtime intercept lifecycle over the admin API** (#493): `POST /intercept` starts the TLS-MITM
  intercept listener at runtime (201 with `{interceptPort, interceptUrl}`, 409 if already running),
  `GET /intercept` reports it (404 when not running), and `DELETE /intercept` stops it (204,
  idempotent). The endpoints are available on every server — a server started **without**
  `--intercept-port` can now enable intercept at runtime (SDK connect/spawn transport parity). The
  CLI flag, the FFI, and these routes all drive one shared listener, so a listener started by any
  surface is visible to the others. New FFI symbol `rift_stop_intercept` mirrors `DELETE /intercept`,
  and `rift_serve_admin` now serves the full `/intercept*` surface against the handle's listener
  (previously it 404'd). All endpoints are gated by `--apikey` like other admin routes.
- **SDK conformance corpus release artifact** (#460, #516, #517): each release now publishes
  `sdk-conformance-<version>.tar.gz` — imposter fixtures with `_verify` transcripts, `data/`, and
  injection modules, plus a README replay contract and a `manifest.json` index — so every SDK's CI
  replays the same fixtures for the engine version it pins and catches DSL/engine drift. An
  engine-side gate replays the whole corpus on every commit, and the SDK-relevant fixtures carry
  self-describing `_verify` sequences so SDKs assert behavior, not just DSL-expressibility.

### Changed
- **Case-insensitive `contains`/`startsWith`/`endsWith` predicates now fold ASCII only** (#480), for
  consistency with `equals` (already ASCII case-insensitive) and to avoid a per-request allocation on
  the matching hot path. Predicates over non-ASCII text that previously matched via Unicode case
  folding (e.g. `É` vs `é`) now require exact non-ASCII bytes; pure-ASCII matching is unchanged.

### Performance
- **Fewer per-request allocations in the imposter matching hot path** (#480, #508): the query string
  is parsed once per request (not once per predicate), case-insensitive string compares no longer
  allocate a lowercased copy of each side, and the request-context header map is captured directly
  instead of being rebuilt and re-validated per request.
- **Scripting hooks run inline on the async worker** (#501): Mountebank `inject`/`decorate`/predicate
  hooks no longer hop to `spawn_blocking` with a per-call timeout; each runs inline with a fresh Boa
  context, removing the blocking-pool round-trip on the scripted hot path.
- **Scenario-FSM Redis I/O offloaded off the tokio worker** (#503): the scenario state machine's
  Redis reads/writes run on `spawn_blocking` and fold `INCRBY`/`EXPIRE` into one round-trip, so a
  slow Redis backend no longer stalls a request worker.
- **HTTP connection pooling re-enabled for proxying** (#496): the proxy client reuses connections
  again (no fresh TCP+TLS handshake per proxied request) and drops a wasted clone of the recorded
  response body.
- **`InMemoryFlowStore` reaps expired entries** (#495): the in-memory flow store now evicts expired
  keys instead of growing unbounded, and `set_ttl` avoids a full-store scan.
- **Request-path regexes routed through the shared cache** (#489): path predicates and route anchors
  reuse cached/`LazyLock` regexes instead of recompiling per request.
- **Per-stub response work precomputed once** (#488): `_behaviors` and non-string bodies are parsed
  and precomputed once per stub rather than re-derived on every matching request.
- **`GET /imposters/{port}/savedRequests?match=` filters before cloning** (#485, #506): a `match=`
  query filters recorded requests over references before cloning, so it no longer deep-clones the
  whole journal to discard most of it.

### Fixed
- **A stub that only names a scenario (`scenarioName`) now gets a real flow store** (#514). Previously
  a stub declaring `scenarioName` without `requiredScenarioState`/`newScenarioState` landed on the
  no-op flow store, so `PUT /imposters/{port}/scenarios/{name}/state`, `POST .../scenarios/reset`
  (and the `rift_set_scenario_state`/`rift_reset_scenarios` FFI symbols) silently discarded the write
  and a subsequent read still showed the initial state. Any stub referencing a scenario now
  provisions an in-memory flow store, so the scenario surface behaves as documented.
- **Proxy `predicateGenerators.inject` failures no longer record a match-all stub** (#498, #507). When
  an inject predicate generator fails (script error, invalid output, script-pool failure, or
  timeout), Rift now skips auto-stub creation instead of silently recording a stub with empty
  predicates that would shadow every future request. The proxied response is still returned and
  carries an `x-rift-generator-error` header naming the failure. A generator that legitimately
  returns `[]` is unchanged.
- **Script timeouts are distinguished from broken-script errors in the response** (#499, #515): an
  `inject`/`decorate` script that times out now maps to `504` while a genuine script error stays
  `400`/`500`, so a slow script and a broken script are no longer conflated in the served response.
- **Per-imposter script state updates are atomic under concurrency** (#477, #486): concurrent
  requests mutating the same imposter's script/flow state no longer lose updates to a read-modify-
  write race.
- **`shellTransform` runs on the blocking pool, not a tokio worker** (#478, #505): the
  `shellTransform` behavior spawns its host subprocess on `spawn_blocking`, so a slow shell command
  can't stall a request worker.
- **Function-form `wait` behavior is clamped/capped like the Boa path** (#490, #504): the regex
  fallback for a JS-function `wait` now clamps and caps the delay the same way the Boa-evaluated path
  does.
- **Every `extern "C"` FFI entry point guards against panic-unwind across the C ABI** (#484, #487): a
  panic inside any FFI symbol is caught and converted to the symbol's error sentinel + `last_error`
  rather than unwinding across the C boundary (undefined behavior).
- **Abnormal intercept accept-loop exits are logged, not swallowed** (#523): a panicked intercept
  accept loop now surfaces its `JoinError` in a warning instead of letting `shutdown`/`stop` report
  success over a crashed listener.

## [0.12.0] - 2026-07-09

This release lands the scripting developer-experience redesign (Script API v2, file-based
authoring, declarative templating, and `rift script` tooling) and **removes the Lua engine** —
consolidating on Rhai and JavaScript. Removing Lua also eliminates the system-LuaJIT dynamic-link
dependency, so the published `librift_ffi` cdylibs are now self-contained on every platform.

### Added
- **Script API v2 — a unified `ctx` object** (#442). Injects and decorates receive a single `ctx`
  with `http(...)`, `delay(...)`, `reset()`, and `pass()` result constructors, replacing the older
  positional calling conventions.
- **Flow-scoped script state** (#443): `ctx.state` with `get_or` / `incr_by` / `cas` / `ttl`,
  backed by an auto-provisioned in-memory store; a configured backend that is down fails loud
  rather than silently dropping state.
- **Declarative response templating** (#444): `{{ … }}` template functions in response bodies and
  headers, behind an opt-in `templated` flag.
- **Script tooling** (#445): `rift script check` and `rift script run` subcommands, plus a
  debug-mode script trace showing which hook ran and its decision.
- **File-based script authoring** (#441): `file:` / `ref:` script references and a named script
  library resolved from `--scripts-dir` (`RIFT_SCRIPTS_DIR`).
- **Mountebank v2 fidelity bundle** (#438): the v2 `config`-first inject convention, a native
  `logger`, script timeouts, `--allowInjection`, error parity, wait functions, and EJS stringify.
- **`request.pathParams`** (#435): route-pattern path-parameter extraction as a stub field, usable
  in predicates and response templating.
- **`ffi-manifest.json` release asset** (#459): a `{version, abi, artifacts[]}` map of the
  `librift_ffi` cdylibs (platform / file / sha256 / url) so SDK consumers resolve natives without
  hardcoding release-URL patterns.
- **x86_64 musl `librift_ffi` cdylib** (#463): a dynamically-linked musl `.so`, for embedded SDK
  tests on Alpine-based CI hosts.
- **`rift_stub_warnings` FFI accessor** (#423): stub-overlap analysis warnings over the C-ABI, so
  embedded consumers get the config-lint the HTTP admin plane already exposed.

### Changed
- **Stub-overlap analysis moved into the engine, cached, and bounded to O(n)** (#423). It is
  computed once on stub mutation and cached — no per-`GET` recompute — exact-duplicate detection is
  O(n) via predicate hashing, and warnings are capped with a `truncated` summary. A multi-tenant
  imposter with hundreds of overlapping stubs no longer stalls admin create/read (seconds and
  hundreds of MB → milliseconds and a few MB), and embedded and standalone share one code path.

### Fixed
- **Predicate-inject errors fail loud** (#440) with a Mountebank-shaped `400`, instead of silently
  falling through.
- **Per-imposter script state is keyed on the bound port** (#439), so auto-bind imposters cannot
  collide in the process-global inject-state map.
- **Released `librift_ffi` cdylibs are self-contained** (#469): a release-time gate asserts each
  cdylib's dynamic imports are stock system libraries only, so they `dlopen` on stock hosts without
  extra packages.

### Removed
- **The Lua scripting engine** (#451) — Rift consolidates on Rhai and JavaScript. This also removes
  the system-LuaJIT dynamic-link dependency that made prior `librift_ffi` cdylibs fail to load on
  hosts without LuaJIT installed. **Breaking:** migrate Lua scripts to Rhai or JavaScript.
- **The v1 `should_inject` scripting contract** (#454). **Breaking:** use the v2 `ctx` API.
- **The dead `RIFT_STRICT_FLOW_STORE` toggle** (#456).

## [0.11.3] - 2026-07-08

### Added
- **`rift_start_intercept` accepts a caller-provided intercept CA** (#429). New optional
  `caCertPath`/`caKeyPath` options load a committed CA (via `CertificateAuthority::load_pem`)
  instead of only minting a fresh ephemeral one, so independent embedded instances can share a
  committed trust anchor — a long-lived containerized SUT can trust a CA that pre-exists its JVM
  startup. A half-configured pair (only one of the two) is rejected with a clear both-or-neither
  error; omitting both generates a CA exactly as before (unchanged default). The load-or-generate
  logic is now shared with the container adapter's `--intercept-ca-cert`/`--intercept-ca-key`.

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

[Unreleased]: https://github.com/EtaCassiopeia/rift/compare/v0.13.1...HEAD
[0.13.1]: https://github.com/EtaCassiopeia/rift/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/EtaCassiopeia/rift/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/EtaCassiopeia/rift/compare/v0.11.3...v0.12.0
[0.11.3]: https://github.com/EtaCassiopeia/rift/compare/v0.11.2...v0.11.3
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
