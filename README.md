# Rift

**High-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust**

[![Status](https://img.shields.io/badge/status-beta-blue)](https://github.com/EtaCassiopeia/rift)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-nightly-orange)](https://www.rust-lang.org/)

Rift is a high-performance, [Mountebank](https://www.mbtest.dev/)-compatible mock server that delivers **~20–150x faster throughput on typical workloads, and up to ~450x on large regex predicate sets**. Use your existing Mountebank configurations and enjoy faster test execution.

**[Documentation](https://etacassiopeia.github.io/rift/)** | **[Quick Start](#quick-start)** | **[Examples](examples/)**

---

## Why Rift?

### Mountebank Compatible

- **Same REST API** - Works with existing Mountebank clients and tooling
- **Same Configuration** - Load your `imposters.json` without changes
- **Same Behavior** - Predicates, responses, behaviors all work identically

### Blazing Fast Performance

| Workload | Mountebank | Rift | Speedup |
|:---------|-----------:|-----:|:--------|
| Simple static stub | 4,141 RPS | 214,028 RPS | **~52x** |
| Deep path match (410 stubs) | 1,342 RPS | 208,580 RPS | **~155x** |
| Complex AND/OR predicates | 4,619 RPS | 191,724 RPS | **~42x** |
| JSON body equals | 7,780 RPS | 203,490 RPS | **~26x** |
| JSONPath predicate | 3,586 RPS | 201,029 RPS | **~56x** |
| XPath predicate | 5,640 RPS | 185,417 RPS | **~33x** |
| Regex path (100 patterns) | 107 RPS | 48,029 RPS | **~449x** |

<sub>Measured 2026-07-07 — Rift `0.11.0` (built from `master`) vs Mountebank `2.9.1`, native
processes (no Docker) on Apple Silicon (macOS), `oha` at 50 keep-alive connections, 20s/scenario
after warmup, each engine run alone on the same machine. Throughput scales with matching
complexity: Rift stays flat while Mountebank's per-request cost grows with stub count and predicate
type. Full methodology and all 13 scenarios: [`tests/benchmark`](tests/benchmark/). Your numbers
will vary with hardware and config.</sub>

### Full Feature Support

- **Imposters** - HTTP/HTTPS mock servers
- **Predicates** - equals, contains, matches, exists, jsonpath, xpath, and, or, not
- **Responses** - Static, proxy, injection
- **Behaviors** - wait, decorate, copy, lookup
- **Proxy Mode** - Record and replay

---

## Quick Start

### Run with Docker

```bash
# Pull and run
docker pull zainalpour/rift-proxy:latest
docker run -p 2525:2525 -p 4545:4545 zainalpour/rift-proxy:latest

# Create your first imposter
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "stubs": [{
      "predicates": [{ "equals": { "path": "/hello" } }],
      "responses": [{ "is": { "statusCode": 200, "body": "Hello, World!" } }]
    }]
  }'

# Test it
curl http://localhost:4545/hello
```

### Use Existing Mountebank Config

```bash
# Load your existing imposters.json
docker run -p 2525:2525 -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json
```

---

## Installation

### Docker (Recommended)

```bash
docker pull zainalpour/rift-proxy:latest
```

### Homebrew (macOS/Linux)

```bash
brew tap etacassiopeia/rift
brew install rift
```

### Cargo (crates.io)

```bash
cargo install rift-http-proxy
```

### Download Binary

Download pre-built binaries from [GitHub Releases](https://github.com/EtaCassiopeia/rift/releases):

```bash
# Example for Linux x86_64
curl -LO https://github.com/EtaCassiopeia/rift/releases/latest/download/rift-vX.X.X-x86_64-unknown-linux-gnu.tar.gz
tar -xzf rift-vX.X.X-x86_64-unknown-linux-gnu.tar.gz
sudo mv rift-vX.X.X-x86_64-unknown-linux-gnu/bin/* /usr/local/bin/
```

Available platforms:
- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc`

### Build from Source

```bash
git clone https://github.com/EtaCassiopeia/rift.git
cd rift
cargo build --release
./target/release/rift-http-proxy
```

### Node.js / npm

For Node.js projects, use the official npm package:

```bash
npm install @rift-vs/rift
```

```javascript
import rift from '@rift-vs/rift';

const server = await rift.create({ port: 2525 });
// Create imposters, run tests...
await server.close();
```

### Java / JVM

For JVM projects, use the official [rift-java](https://github.com/EtaCassiopeia/rift-java) SDK.
It runs the engine three ways — embedded in-process (Panama FFM, no Docker), connected to any
running admin endpoint, or as a managed spawned binary — with a fluent DSL plus JUnit 5, Spring,
and Testcontainers integrations. Available on Maven Central under `io.github.etacassiopeia`:

```xml
<dependency>
  <groupId>io.github.etacassiopeia</groupId>
  <artifactId>rift-java-core</artifactId>
  <scope>test</scope>
</dependency>
```

```java
try (Rift rift = Rift.embedded()) {                  // or Rift.connect(uri) / Rift.spawn()
    Imposter users = rift.create(
        imposter("users").stub(onGet("/api/users/1").willReturn(okJson("{\"id\":1}"))));
    // point your SUT at users.uri(), then assert:
    users.verify(onGet("/api/users/1"), times(1));
}
```

See the [rift-java docs](https://etacassiopeia.github.io/rift-java/) for the full feature surface.

---

## Documentation

### Getting Started
- [Installation](https://etacassiopeia.github.io/rift/getting-started/) - Docker, binary, build from source
- [Quick Start](https://etacassiopeia.github.io/rift/getting-started/quickstart) - Create your first imposter
- [Node.js Integration](https://etacassiopeia.github.io/rift/getting-started/nodejs/) - npm package for Node.js
- [Java / JVM SDK](https://github.com/EtaCassiopeia/rift-java) - rift-java for JUnit 5, Spring, and Testcontainers
- [Migration Guide](https://etacassiopeia.github.io/rift/getting-started/migration) - Using Rift with Mountebank configs

### Mountebank Compatibility
- [Imposters](https://etacassiopeia.github.io/rift/mountebank/imposters) - Mock server configuration
- [Predicates](https://etacassiopeia.github.io/rift/mountebank/predicates) - Request matching
- [Responses](https://etacassiopeia.github.io/rift/mountebank/responses) - Response configuration
- [Behaviors](https://etacassiopeia.github.io/rift/mountebank/behaviors) - wait, decorate, copy
- [Proxy Mode](https://etacassiopeia.github.io/rift/mountebank/proxy) - Record and replay

### Configuration
- [Mountebank Format](https://etacassiopeia.github.io/rift/configuration/mountebank) - JSON configuration
- [Native Rift Format](https://etacassiopeia.github.io/rift/configuration/native) - YAML for advanced features
- [CLI Reference](https://etacassiopeia.github.io/rift/configuration/cli) - Command-line options

### Features
- [Fault Injection](https://etacassiopeia.github.io/rift/features/fault-injection) - Chaos engineering
- [Scripting](https://etacassiopeia.github.io/rift/features/scripting) - Rhai, Lua, JavaScript
- [TLS/HTTPS](https://etacassiopeia.github.io/rift/features/tls) - Secure connections
- [Metrics](https://etacassiopeia.github.io/rift/features/metrics) - Prometheus integration
- [TUI](https://etacassiopeia.github.io/rift/features/tui) - Interactive terminal interface

### Deployment
- [Docker](https://etacassiopeia.github.io/rift/deployment/docker) - Container deployment
- [Kubernetes](https://etacassiopeia.github.io/rift/deployment/kubernetes) - K8s patterns

### Reference
- [REST API](https://etacassiopeia.github.io/rift/api/) - Admin API reference
- [Performance](https://etacassiopeia.github.io/rift/performance/) - Benchmarks

---

## Example

```json
{
  "port": 4545,
  "protocol": "http",
  "name": "User Service",
  "stubs": [
    {
      "predicates": [{ "equals": { "method": "GET", "path": "/users" } }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": [{ "id": 1, "name": "Alice" }]
        }
      }]
    },
    {
      "predicates": [{
        "and": [
          { "equals": { "method": "GET" } },
          { "matches": { "path": "/users/\\d+" } }
        ]
      }],
      "responses": [{
        "is": { "statusCode": 200, "body": { "id": 1, "name": "Alice" } }
      }]
    }
  ]
}
```

More examples in [`examples/`](examples/).

---

## Metrics

Prometheus metrics on `:9090/metrics`:

```bash
curl http://localhost:9090/metrics
```

Metrics include request counts, latency histograms, fault injection stats, and more.

---

## CLI Tools

Rift includes additional command-line tools. All tools are included when you install via Homebrew or download release binaries.

### rift-tui - Interactive Terminal UI

Manage imposters and stubs through an interactive terminal interface:

```bash
# If installed via Homebrew or release binary
rift-tui

# Connect to a different admin URL
rift-tui --admin-url http://localhost:2525
```

Features:
- View and manage imposters with vim-style navigation (j/k)
- Create, edit, and delete stubs with JSON editor
- Generate curl commands for testing stubs
- Import/export imposter configurations
- Search and filter imposters and stubs
- Real-time metrics dashboard

### rift-verify - Stub Verification

Automatically test your imposters by generating requests from predicates:

```bash
rift-verify --show-curl
```

### rift-lint - Configuration Linter

Validate imposter configuration files before loading:

```bash
# If installed via Homebrew or release binary
rift-lint ./imposters/

# Via Docker (for CI/CD)
docker run --rm -v $(pwd):/imposters zainalpour/rift-lint .

# Via cargo
cargo install rift-lint
rift-lint ./imposters/
```

---

## Development

```bash
# Build
cargo build --release

# Run tests
cargo test --all

# Run with debug logging
RUST_LOG=debug ./target/release/rift-http-proxy

# Run benchmarks (Rift vs Mountebank; see tests/benchmark/README.md)
cd tests/benchmark && python3 scripts/bench_direct.py --run-all \
  --rift-bin ../../target/release/rift-http-proxy \
  --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb
```

---

## Contributing

Contributions welcome! Please read our contributing guidelines and submit PRs.

---

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

---

## Acknowledgments

- [Mountebank](http://www.mbtest.org/) - The original service virtualization tool that inspired Rift's API and configuration format
