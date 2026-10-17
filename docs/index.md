---
title: Home
description: Rift is a high-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust
---

# Rift

**High-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust**

Rift is a drop-in replacement for [Mountebank](https://www.mbtest.dev/) that delivers **~20–150x faster throughput on typical workloads, and up to ~1,820x on large regex predicate sets** — while maintaining full API compatibility. Use your existing Mountebank configurations and enjoy faster test execution.

Rift is compatible with **Mountebank 2.9.5**, the latest release ([`@mbtest/mountebank`](https://www.npmjs.com/package/@mbtest/mountebank)), checked against a real Mountebank on every pull request that changes the engine — see [Differences from Mountebank](mountebank/differences.md) for the few deliberate exceptions.

[Get Started](getting-started/index.md){ .md-button .md-button--primary }
[View on GitHub](https://github.com/achird-labs/rift){ .md-button }

---

## Why Rift?

### Drop-in Mountebank Replacement

Rift implements the Mountebank REST API, allowing you to:

- Use existing Mountebank configuration files without changes
- Keep your current test infrastructure and tooling
- Switch between Mountebank and Rift transparently

### Blazing Fast Performance

Built in Rust with async I/O, Rift delivers exceptional performance:

| Feature | Mountebank | Rift | Speedup |
|:--------|:-----------|:-----|:--------|
| Static stub (first match) | 7,240 RPS | 209,192 RPS | **29x faster** |
| Regex (100th pattern) | 110 RPS | 201,067 RPS | **1,821x faster** |
| JSONPath predicates | 4,586 RPS | 190,329 RPS | **42x faster** |
| API stub — no match (404) | 1,376 RPS | 204,358 RPS | **149x faster** |
| Complex predicates | 4,884 RPS | 185,867 RPS | **38x faster** |

<sub>Apple M4 laptop, 50 connections, median of 3 repetitions, 2026-09-17. On a 16-vCPU AMD EPYC server the
same suite reaches 325k RPS and 6,160x on regex — but Mountebank is *slower* there, so the M4
figures above are the conservative read.</sub>

See the [performance page](performance/index.md) for both hosts, the full suite, and the method.

### Full Feature Compatibility

Rift supports all major Mountebank features:

- **Imposters** - HTTP/HTTPS mock servers on any port, including mutual TLS
- **Stubs** - Request matching with responses
- **Predicates** - equals, contains, matches, exists, jsonpath, xpath, and, or, not
- **Responses** - Static, proxy, injection with behaviors
- **Behaviors** - wait, decorate, copy, lookup, shellTransform
- **Recording** - Proxy mode with response recording

### And Then Some

Compatibility is the floor, not the ceiling. Rift adds the things you would otherwise build by hand
on top of a mock server:

- **[Fault Injection](features/fault-injection.md)** - probabilistic latency, error and TCP faults, declaratively
- **[Scripting](features/scripting.md)** - Rhai and JavaScript engines, with a `script check` / `script run` CLI
- **[Scenarios](features/scenarios.md)** and **[Flow State](features/flow-state.md)** - declarative state machines and a per-flow key/value store
- **[Correlated Isolation](features/spaces.md)** - per-flow partitioning so parallel tests don't collide
- **[Front Door](features/front-door.md)** and **[Gateway](features/gateway.md)** - route many imposters through one listener
- **[Intercept Proxy](features/intercept-proxy.md)** - TLS-MITM a hard-coded external host without mitmproxy, over HTTP/1.1 or HTTP/2, with WebSocket upgrades relayed to the real origin
- **[TLS & Mutual TLS](features/tls.md)** - HTTPS imposters that require and validate client certificates, plus a private-CA trust store for proxying
- **[Response Templates](features/date-templates.md)** - date tokens and the `_rift.templated` `{{ }}` grammar, no script engine needed
- **[Stub Analysis](features/stub-analysis.md)** and **[Debug Mode](features/debug-mode.md)** - find shadowed stubs, and see why a request matched
- **[Conditional GET](mountebank/responses.md#conditional-get)** and **[Vendor-Mock Templates](templates/index.md)** - `ETag` / `304` without hand-synced validators, and ready-made mocks for third-party SaaS
- **[Embedding & FFI](embedding/index.md)** - run the engine in-process from Rust or any language over the C ABI

The [Features](features/index.md) section covers all of them.

---

## Quick Start

### Using Docker (Recommended)

```bash
# Pull the latest image from Docker Hub
docker pull zainalpour/rift-proxy:latest

# Run Rift (Mountebank-compatible mode)
docker run -p 2525:2525 zainalpour/rift-proxy:latest
```

### Create Your First Imposter

```bash
# Create an imposter that responds to GET /hello
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "predicates": [{ "equals": { "method": "GET", "path": "/hello" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "Hello, World!" } }]
    }]
  }'

# Test the imposter
curl http://localhost:4545/hello
# Output: Hello, World!
```

### Using an Existing Mountebank Config

```bash
# Start Rift with your existing imposters.json file
docker run -p 2525:2525 -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

### Node.js Integration

For Node.js projects, use the official npm package:

```bash
npm install @rift-vs/rift
```

```javascript
import { rift, imposter, onGet, okJson, times } from '@rift-vs/rift';

await using engine = await rift.embedded(); // or rift.connect(url) / rift.spawn()

const users = await engine.create(
  imposter('users').stub(onGet('/api/users/1').willReturn(okJson({ id: 1, name: 'Alice' }))));

await fetch(`${users.url}/api/users/1`);
await users.verify(onGet('/api/users/1'), times(1)); // throws with a diff on mismatch
```

Migrating from Mountebank? The Mountebank-compatible `rift.create({ port })` API stays available as
a permanent drop-in, so adopting the typed DSL above is incremental rather than a rewrite.

See the [Node.js Integration Guide](getting-started/nodejs.md) for complete documentation.

### Java / JVM Integration

For JVM projects, use the official [rift-java](https://github.com/achird-labs/rift-java) SDK. It runs
the engine three ways — embedded in-process (Panama FFM, no Docker), connected to any running admin
endpoint, or as a managed spawned binary — with a fluent DSL plus JUnit 5, Spring, and Testcontainers
integrations.

```xml
<dependency>
  <groupId>io.github.achird-labs</groupId>
  <artifactId>rift-java-core</artifactId>
  <scope>test</scope>
</dependency>
```

```java
try (Rift rift = Rift.embedded()) {                  // or Rift.connect(uri) / Rift.spawn()
    Imposter users = rift.create(
        imposter("users").stub(onGet("/api/users/1").willReturn(okJson("{\"id\":1}"))));
    // point your system under test at users.uri(), then assert:
    users.verify(onGet("/api/users/1"), times(1));
}
```

See the [rift-java documentation](https://achird-labs.github.io/rift-java/) for the full feature
surface, and the [BOM](https://github.com/achird-labs/rift-java/blob/master/rift-java-bom/README.md)
for version-pinning every module at once.

### Go Integration

For Go projects, use the official [rift-go](https://github.com/achird-labs/rift-go) SDK. It runs the
engine three ways — embedded in-process, connected to any running admin endpoint, or as a managed
spawned binary — with a fluent DSL plus `testing.T` helpers.

The embedded transport loads the engine through
[purego](https://github.com/ebitengine/purego) rather than cgo, so **`CGO_ENABLED=0` keeps
working**: no C toolchain, and cross-compilation is unaffected.

```bash
go get github.com/achird-labs/rift-go
go run github.com/achird-labs/rift-go/cmd/rift-fetch@latest -version v0.23.0
```

```go
func TestUserLookup(t *testing.T) {
    users := rifttest.Imposter(t, rift.NewImposter("users").
        Stub(rift.OnGet("/api/users/1").
            Return(rift.OKJSON(map[string]rift.JSON{"id": 1, "name": "Alice"}))))

    callSUT(t, users.BaseURL())

    rifttest.AssertReceived(t, users, rift.OnGet("/api/users/1"), rift.Once())
}
```

See the [rift-go documentation](https://achird-labs.github.io/rift-go/) for the full feature
surface.

### All four SDKs

Java, Scala, Node/TypeScript and Go are all officially supported and all replay the same
conformance corpus. The [Language SDKs](sdk/index.md) section has the install snippet and
hello-world for each, plus the transport and version-compatibility matrices.

---

## Documentation

### Getting Started
- [Installation](getting-started/index.md) - Docker, binary, and build from source
- [Quick Start](getting-started/quickstart.md) - Create your first imposter
- [Node.js Integration](getting-started/nodejs.md) - npm package for Node.js projects
- [Language SDKs](sdk/index.md) - Java, Scala, Node/TypeScript and Go, with the transport and version-compatibility matrices
- [Java / JVM SDK](sdk/java.md) - rift-java for JUnit 5, Spring, and Testcontainers
- [Scala SDK](sdk/scala.md) - rift-scala for ZIO, Cats Effect, FS2, and zio-bdd
- [Go SDK](sdk/go.md) - rift-go for `testing.T`, embedded via purego (no cgo)
- [Migration from Mountebank](getting-started/migration.md) - Switch from Mountebank to Rift

### Concepts
- [Concepts Overview](concepts/index.md) - The Rift mental model, start here
- [Core Building Blocks](concepts/building-blocks.md) - Imposters, stubs, predicates, responses, behaviors
- [The Rift Model](concepts/rift-model.md) - Flow-state, scenarios, and correlated isolation

### Mountebank Compatibility (reference)
- [Imposters](mountebank/imposters.md) - Creating and managing mock servers
- [Predicates](mountebank/predicates.md) - Request matching (equals, contains, regex, jsonpath, xpath)
- [Responses](mountebank/responses.md) - Configuring stub responses
- [Behaviors](mountebank/behaviors.md) - Response modification (wait, decorate, copy, lookup, shellTransform)
- [Proxy Mode](mountebank/proxy.md) - Recording and replaying responses

### Configuration
- [Mountebank Format](configuration/mountebank.md) - JSON configuration reference
- [Rift Extensions](configuration/native.md) - The `_rift` namespace and other Rift-specific keys
- [CLI Reference](configuration/cli.md) - Command-line options

### Features
- [Features Overview](features/index.md) - Every extension, with the Mountebank comparison table
- [Fault Injection](features/fault-injection.md) - Latency and error simulation
- [Scripting](features/scripting.md) - Rhai and JavaScript engines
- [Scenarios (FSM)](features/scenarios.md) - Stateful stubs as declarative state machines
- [Correlated Isolation (Spaces)](features/spaces.md) - Per-flow stub and state partitioning
- [Flow State](features/flow-state.md) - Per-flow key/value store
- [Front Door](features/front-door.md) - One listener routing to many imposters
- [Single-Port Gateway](features/gateway.md) - Reach every imposter through the admin port
- [Intercept Proxy (TLS-MITM)](features/intercept-proxy.md) - Mock a hard-coded external HTTPS host (HTTP/1.1, HTTP/2, WebSocket passthrough)
- [Response Templates](features/date-templates.md) - Date tokens and the `_rift.templated` grammar
- [Hot Reload](features/hot-reload.md) - Re-read config without restarting
- [Stub Analysis](features/stub-analysis.md) - Overlap detection and warnings
- [Debug Mode](features/debug-mode.md) - Why a request matched, or didn't
- [TLS/HTTPS](features/tls.md) - HTTPS imposters, mutual TLS, outbound trust for private CAs
- [Metrics](features/metrics.md) - Prometheus integration
- [Configuration Linting](features/linting.md) - Validate configs before they load
- [Terminal UI](features/tui.md) - Interactive imposter management

### Deployment
- [Docker](deployment/docker.md) - Container deployment
- [Kubernetes](deployment/kubernetes.md) - K8s deployment patterns
- [ECS / Fargate](deployment/ecs-fargate.md) - Reference task definition, including the intercept proxy

### Reference
- [REST API](api/index.md) - Admin API reference
- [Examples](examples/index.md) - Ready-to-use imposter configurations
- [Templates](templates/index.md) - Vendor-mock templates for third-party SaaS
- [Performance](performance/index.md) - Benchmark results
- [Rift vs WireMock](comparisons/wiremock.md) - Where each one wins, and when not to switch
- [Rift vs Microcks](comparisons/microcks.md) - Spec-driven vs stub-driven, and where they overlap
- [Changelog](changelog.md) - Notable user-facing changes

### Embedding & Extension
- [Embedding & SPI](embedding/index.md) - Embed Rift as a library, extend it via SPI traits
- [Embeddable Server](embedding/server.md) - `ServerBuilder`, bindable admin/metrics
- [Extension Points (SPI)](embedding/spi.md) - Pluggable flow-store, journal, proxy store, sequencer, plus exchange-inspector, no-match, admin-authorizer and front-door observer hooks
- [FFI (C-ABI)](embedding/ffi.md) - Drive Rift from any language

---

## Project Status

The HTTP/HTTPS surface is stable and actively developed. Current status:

| Area | Status |
|:-----|:-------|
| HTTP / HTTPS imposters, including mutual TLS | Stable |
| All predicates, static responses, behaviors | Stable |
| Proxy mode (record & replay) | Stable |
| JavaScript injection | Stable |
| Fault injection, scripting (Rhai / JS) | Stable |
| Scenarios, flow state, correlated isolation | Stable |
| Front door, single-port gateway | Stable |
| Intercept proxy (TLS-MITM, HTTP/2, WebSocket passthrough) | Stable |
| Stub analysis, debug mode, hot reload | Stable |
| Prometheus metrics, linting, terminal UI | Stable |
| Embedding (Rust API, C ABI) and the four SDKs | Stable |
| TCP protocol | Not supported |
| SMTP protocol | Not supported |

---

## License

Rift is distributed under the [Apache License 2.0](https://github.com/achird-labs/rift/blob/master/LICENSE).
