---
layout: default
title: Home
nav_order: 1
description: "Rift is a high-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust"
permalink: /
---

# Rift

**High-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust**
{: .fs-6 .fw-300 }

Rift is a drop-in replacement for [Mountebank](http://www.mbtest.org/) that provides **2-250x better performance** while maintaining full API compatibility. Use your existing Mountebank configurations and enjoy faster test execution.

[Get Started]({{ site.baseurl }}/getting-started/){: .btn .btn-primary .fs-5 .mb-4 .mb-md-0 .mr-2 }
[View on GitHub](https://github.com/achird-labs/rift){: .btn .fs-5 .mb-4 .mb-md-0 }

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
| Simple stubs | 8,898 RPS | 214,818 RPS | **24x faster** |
| Regex (100th pattern) | 112 RPS | 207,024 RPS | **1,857x faster** |
| JSONPath predicates | 4,312 RPS | 199,404 RPS | **46x faster** |
| API stub — no match (404) | 1,351 RPS | 209,763 RPS | **155x faster** |
| Complex predicates | 4,703 RPS | 191,987 RPS | **41x faster** |

<sub>Apple M4 laptop, 50 connections, median of 3 repetitions. On a 16-vCPU AMD EPYC server the
same suite reaches 325k RPS and 6,160x on regex — but Mountebank is *slower* there, so the M4
figures above are the conservative read.</sub>

See the [performance page](performance/) for both hosts, the full suite, and the method.

### Full Feature Compatibility

Rift supports all major Mountebank features:

- **Imposters** - HTTP/HTTPS mock servers on any port
- **Stubs** - Request matching with responses
- **Predicates** - equals, contains, matches, exists, jsonpath, xpath, and, or, not
- **Responses** - Static, proxy, injection with behaviors
- **Behaviors** - wait, decorate, copy, lookup
- **Recording** - Proxy mode with response recording

---

## Quick Start

### Using Docker (Recommended)

```bash
# Pull the latest image (from GitHub Container Registry)
docker pull zainalpour/rift-proxy:latest

# Or from Docker Hub
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
import rift from '@rift-vs/rift';

// Start a Rift server (drop-in replacement for Mountebank)
const server = await rift.create({ port: 2525 });

// Create imposters, run your tests...

await server.close();
```

See the [Node.js Integration Guide]({{ site.baseurl }}/getting-started/nodejs/) for complete documentation.

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

---

## Documentation

### Getting Started
- [Installation]({{ site.baseurl }}/getting-started/) - Docker, binary, and build from source
- [Quick Start]({{ site.baseurl }}/getting-started/quickstart/) - Create your first imposter
- [Node.js Integration]({{ site.baseurl }}/getting-started/nodejs/) - npm package for Node.js projects
- [Java / JVM SDK](https://github.com/achird-labs/rift-java) - rift-java for JUnit 5, Spring, and Testcontainers
- [Scala SDK](https://github.com/achird-labs/rift-scala) - rift-scala for ZIO, Cats Effect, FS2, and zio-bdd
- [Migration from Mountebank]({{ site.baseurl }}/getting-started/migration/) - Switch from Mountebank to Rift

### Concepts
- [Concepts Overview]({{ site.baseurl }}/concepts/) - The Rift mental model, start here
- [Core Building Blocks]({{ site.baseurl }}/concepts/building-blocks/) - Imposters, stubs, predicates, responses, behaviors
- [The Rift Model]({{ site.baseurl }}/concepts/rift-model/) - Flow-state, scenarios, and correlated isolation

### Mountebank Compatibility (reference)
- [Imposters]({{ site.baseurl }}/mountebank/imposters/) - Creating and managing mock servers
- [Predicates]({{ site.baseurl }}/mountebank/predicates/) - Request matching (equals, contains, regex, jsonpath, xpath)
- [Responses]({{ site.baseurl }}/mountebank/responses/) - Configuring stub responses
- [Behaviors]({{ site.baseurl }}/mountebank/behaviors/) - Response modification (wait, decorate, copy)
- [Proxy Mode]({{ site.baseurl }}/mountebank/proxy/) - Recording and replaying responses

### Configuration
- [Mountebank Format]({{ site.baseurl }}/configuration/mountebank/) - JSON configuration reference
- [Native Rift Format]({{ site.baseurl }}/configuration/native/) - YAML configuration for advanced features
- [CLI Reference]({{ site.baseurl }}/configuration/cli/) - Command-line options

### Features
- [Fault Injection]({{ site.baseurl }}/features/fault-injection/) - Latency and error simulation
- [Scripting]({{ site.baseurl }}/features/scripting/) - Rhai and JavaScript engines
- [TLS/HTTPS]({{ site.baseurl }}/features/tls/) - Secure connections
- [Metrics]({{ site.baseurl }}/features/metrics/) - Prometheus integration

### Deployment
- [Docker]({{ site.baseurl }}/deployment/docker/) - Container deployment
- [Kubernetes]({{ site.baseurl }}/deployment/kubernetes/) - K8s deployment patterns

### Reference
- [REST API]({{ site.baseurl }}/api/) - Admin API reference
- [Performance]({{ site.baseurl }}/performance/) - Benchmark results
- [Changelog]({{ site.baseurl }}/changelog/) - Notable user-facing changes

### Embedding & Extension
- [Embedding & SPI]({{ site.baseurl }}/embedding/) - Embed Rift as a library, extend it via SPI traits
- [Embeddable Server]({{ site.baseurl }}/embedding/server/) - `ServerBuilder`, bindable admin/metrics
- [Extension Points (SPI)]({{ site.baseurl }}/embedding/spi/) - Pluggable flow-store, journal, proxy store, sequencer
- [FFI (C-ABI)]({{ site.baseurl }}/embedding/ffi/) - Drive Rift from any language

---

## Project Status

Rift is under active development. Current status:

| Feature | Status |
|:--------|:-------|
| HTTP Imposters | Stable |
| HTTPS Imposters | Stable |
| All Predicates | Stable |
| Static Responses | Stable |
| Proxy Mode | Stable |
| Behaviors (wait, decorate) | Stable |
| Injection (JavaScript) | Stable |
| TCP Protocol | Planned |
| SMTP Protocol | Planned |

---

## License

Rift is distributed under the [Apache License 2.0](https://github.com/achird-labs/rift/blob/master/LICENSE).
