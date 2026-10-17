# Features

Rift provides advanced features for service virtualization and chaos engineering.

---

## Core Features

### Mountebank Compatibility

- **Imposters** - Mock HTTP/HTTPS servers
- **Mutual TLS** - Require, and optionally validate, a client certificate
- **Predicates** - Flexible request matching
- **Responses** - Static, proxy, and dynamic responses
- **Behaviors** - Response modification and delays
- **JavaScript Injection** - Dynamic response generation

### Rift Extensions (`_rift` Namespace)

- **Fault Injection** - Probabilistic latency, error, and TCP fault injection
- **Scripting** - Rhai and JavaScript engines for dynamic behavior
- **Response Templates** - Date tokens and the `_rift.templated` `{{ }}` function grammar
- **Conditional GET** - `ETag` / `Last-Modified` and `304 Not Modified` on an `is` response with `_rift.conditional`
- **Scenarios (FSM)** - Stateful stubs as declarative state machines
- **Flow State** - Per-flow key/value store with the in-memory backend, written by scripts or declaratively with `_rift.stateOps`
- **Correlated Isolation (Spaces)** - Per-flow stub and state partitioning
- **Stub Analysis** - Overlap detection and conflict warnings
- **Debug Mode** - Request matching diagnostics with `X-Rift-Debug` header
- **Metrics** - Prometheus integration

### Server-Level Capabilities

- **Front Door** - One listener routing to many imposters by host, path, header or method
- **Single-Port Gateway** - Reach every imposter through the admin port
- **Intercept Proxy** - TLS-MITM a hard-coded external HTTPS host, no mitmproxy needed
- **Hot Reload** - Re-read configuration without restarting the process
- **Outbound TLS Trust** - Reach an origin behind a private CA when proxying or recording
- **Imposter Sources** - Load from a path or a URI, merging several sources
- **Embedding** - Run the engine in-process from Rust, or any language over the C ABI

---

## Feature Overview

| Feature | Mountebank | Rift Extensions |
|:--------|:-----------|:----------------|
| HTTP/HTTPS Mocking | ✅ Full support | — |
| Mutual TLS (client certs) | ⚠️ Accepted, never enforced | ✅ `mutualAuth` / `rejectUnauthorized` / `ca` |
| Outbound TLS trust (private CAs) | — | ✅ `--upstream-ca-file` |
| Request Matching | ✅ Full predicates | — |
| Static Responses | ✅ | — |
| Proxy Recording | ✅ | — |
| JavaScript Injection | ✅ | — |
| Probabilistic Faults | Via injection | ✅ `_rift.fault` |
| Rhai/JS Scripting | — | ✅ `_rift.script` |
| Response Templates | `${request.*}` | ✅ `{{NOW}}`, `_rift.templated` |
| Conditional GET (`ETag`, `304`) | Two stubs, hand-synced | ✅ `_rift.conditional` |
| Scenarios (FSM) | Via injection | ✅ stub `scenarioName` |
| Flow State | Via injection | ✅ `_rift.flowState` |
| Correlated Isolation | — | ✅ stub `space` |
| Stub Analysis | — | ✅ `_rift.warnings` |
| Stub IDs | — | ✅ `id` field |
| Debug Mode | — | ✅ `X-Rift-Debug` header |
| Prometheus Metrics | ✅ | ✅ |
| Front Door | — | ✅ `--front-door` |
| Single-Port Gateway | — | ✅ via the admin port |
| Intercept Proxy (TLS-MITM) | — | ✅ `--intercept-port` |
| Hot Reload | — | ✅ `POST /admin/reload` |
| Config Linting | — | ✅ `rift-lint` |
| Stub Verification | — | ✅ `rift-verify` |
| Terminal UI | — | ✅ `rift-tui` |
| In-Process Embedding | — | ✅ Rust API + C ABI |
| Official Typed SDKs | — | ✅ Java, Scala, Node, Go |

---

## Feature Documentation

- [Fault Injection](fault-injection.md) - Latency and error simulation
- [Scripting](scripting.md) - Dynamic behavior with scripts
- [Scenarios (FSM)](scenarios.md) - Stateful stubs with declarative state machines
- [Correlated Isolation (Spaces)](spaces.md) - Per-flow stub and state partitioning
- [Flow State](flow-state.md) - Per-flow key/value store for stateful mocks
- [Response Templates](date-templates.md) - date tokens and the `_rift.templated` `{{ }}` function grammar
- [Conditional GET](../mountebank/responses.md#conditional-get) - `ETag`, `Last-Modified` and `304` with `_rift.conditional`
- [Stub-by-ID](stub-by-id.md) - Address stubs by stable id
- [Single-Port Gateway](gateway.md) - Reach every imposter through the admin port
- [Front Door](front-door.md) - One listener routing to many imposters by host, path, header or method
- [Hot Reload](hot-reload.md) - Re-read config without restarting
- [Stub Analysis](stub-analysis.md) - Overlap detection and warnings
- [Debug Mode](debug-mode.md) - Request matching diagnostics
- [TLS/HTTPS](tls.md) - Secure connections
- [Intercept Proxy (TLS-MITM)](intercept-proxy.md) - Mock a hard-coded external HTTPS host without mitmproxy
- [Metrics](metrics.md) - Prometheus monitoring
- [Configuration Linting](linting.md) - Validate imposter configs before loading
- [Terminal UI](tui.md) - Interactive imposter management

Documented elsewhere:

- [Outbound TLS trust](tls.md#trusting-a-private-ca) - `--upstream-ca-file`
- [Imposter Sources](../configuration/cli.md#imposter-sources) - `--imposters` URIs and merging
- [Embedding](../embedding/index.md) - Rust API and C ABI
