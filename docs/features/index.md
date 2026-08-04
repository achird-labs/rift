---
layout: default
title: Features
nav_order: 5
has_children: true
permalink: /features/
---

# Features

Rift provides advanced features for service virtualization and chaos engineering.

---

## Core Features

### Mountebank Compatibility

- **Imposters** - Mock HTTP/HTTPS servers
- **Predicates** - Flexible request matching
- **Responses** - Static, proxy, and dynamic responses
- **Behaviors** - Response modification and delays
- **JavaScript Injection** - Dynamic response generation

### Rift Extensions (`_rift` Namespace)

- **Fault Injection** - Probabilistic latency, error, and TCP fault injection
- **Scripting** - Rhai and JavaScript engines for dynamic behavior
- **Scenarios (FSM)** - Stateful stubs as declarative state machines
- **Flow State** - Per-flow key/value store with InMemory or Redis backends
- **Correlated Isolation (Spaces)** - Per-flow stub and state partitioning
- **Stub Analysis** - Overlap detection and conflict warnings
- **Debug Mode** - Request matching diagnostics with `X-Rift-Debug` header
- **Metrics** - Prometheus integration

### Server-Level Capabilities

- **Front Door** - One listener routing to many imposters by host, path, header or method
- **Single-Port Gateway** - Reach every imposter through the admin port
- **Intercept Proxy** - TLS-MITM a hard-coded external HTTPS host, no mitmproxy needed
- **Hot Reload** - Re-read configuration without restarting the process
- **Imposter Sources** - Load from a path or a URI, merging several sources
- **Embedding** - Run the engine in-process from Rust, or any language over the C ABI

---

## Feature Overview

| Feature | Mountebank | Rift Extensions |
|:--------|:-----------|:----------------|
| HTTP/HTTPS Mocking | ✅ Full support | — |
| Request Matching | ✅ Full predicates | — |
| Static Responses | ✅ | — |
| Proxy Recording | ✅ | — |
| JavaScript Injection | ✅ | — |
| Probabilistic Faults | Via injection | ✅ `_rift.fault` |
| Rhai/JS Scripting | — | ✅ `_rift.script` |
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

- [Fault Injection]({{ site.baseurl }}/features/fault-injection/) - Latency and error simulation
- [Scripting]({{ site.baseurl }}/features/scripting/) - Dynamic behavior with scripts
- [Scenarios (FSM)]({{ site.baseurl }}/features/scenarios/) - Stateful stubs with declarative state machines
- [Correlated Isolation (Spaces)]({{ site.baseurl }}/features/spaces/) - Per-flow stub and state partitioning
- [Flow State]({{ site.baseurl }}/features/flow-state/) - Per-flow key/value store for stateful mocks
- [Date Templates]({{ site.baseurl }}/features/date-templates/) - `{{NOW}}` / `{{DAYS±N}}` / `{{MONTHS±N}}` in responses
- [Stub-by-ID]({{ site.baseurl }}/features/stub-by-id/) - Address stubs by stable id
- [Single-Port Gateway]({{ site.baseurl }}/features/gateway/) - Reach every imposter through the admin port
- [Front Door]({{ site.baseurl }}/features/front-door/) - One listener routing to many imposters by host, path, header or method
- [Hot Reload]({{ site.baseurl }}/features/hot-reload/) - Re-read config without restarting
- [Stub Analysis]({{ site.baseurl }}/features/stub-analysis/) - Overlap detection and warnings
- [Debug Mode]({{ site.baseurl }}/features/debug-mode/) - Request matching diagnostics
- [TLS/HTTPS]({{ site.baseurl }}/features/tls/) - Secure connections
- [Intercept Proxy (TLS-MITM)]({{ site.baseurl }}/features/intercept-proxy/) - Mock a hard-coded external HTTPS host without mitmproxy
- [Metrics]({{ site.baseurl }}/features/metrics/) - Prometheus monitoring
- [Configuration Linting]({{ site.baseurl }}/features/linting/) - Validate imposter configs before loading
- [Terminal UI]({{ site.baseurl }}/features/tui/) - Interactive imposter management
