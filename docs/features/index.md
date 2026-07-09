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
- **Flow State** - Stateful scenarios with InMemory or Redis backends
- **Stub Analysis** - Overlap detection and conflict warnings
- **Debug Mode** - Request matching diagnostics with `X-Rift-Debug` header
- **Metrics** - Prometheus integration

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
| Flow State | Via injection | ✅ `_rift.flowState` |
| Stub Analysis | — | ✅ `_rift.warnings` |
| Stub IDs | — | ✅ `id` field |
| Debug Mode | — | ✅ `X-Rift-Debug` header |
| Prometheus Metrics | ✅ | ✅ |
| Config Linting | — | ✅ `rift-lint` |
| Terminal UI | — | ✅ `rift-tui` |

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
- [Hot Reload]({{ site.baseurl }}/features/hot-reload/) - Re-read config without restarting
- [Stub Analysis]({{ site.baseurl }}/features/stub-analysis/) - Overlap detection and warnings
- [Debug Mode]({{ site.baseurl }}/features/debug-mode/) - Request matching diagnostics
- [TLS/HTTPS]({{ site.baseurl }}/features/tls/) - Secure connections
- [Intercept Proxy (TLS-MITM)]({{ site.baseurl }}/features/intercept-proxy/) - Mock a hard-coded external HTTPS host without mitmproxy
- [Metrics]({{ site.baseurl }}/features/metrics/) - Prometheus monitoring
- [Configuration Linting]({{ site.baseurl }}/features/linting/) - Validate imposter configs before loading
- [Terminal UI]({{ site.baseurl }}/features/tui/) - Interactive imposter management
