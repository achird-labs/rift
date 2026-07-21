# Rift

**High-performance Mountebank-compatible HTTP/HTTPS mock server written in Rust**

[![Status](https://img.shields.io/badge/status-beta-blue)](https://github.com/achird-labs/rift)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-nightly-orange)](https://www.rust-lang.org/)

Rift is a high-performance, [Mountebank](https://www.mbtest.dev/)-compatible mock server that delivers **~20–150x faster throughput on typical workloads, and up to ~450x on large regex predicate sets**. Use your existing Mountebank configurations and enjoy faster test execution.

**[Documentation](https://achird-labs.github.io/rift/)** | **[Quick Start](#quick-start)** | **[Examples](examples/)**

---

## Why Rift?

### Mountebank Compatible

- **Same REST API** - Works with existing Mountebank clients and tooling
- **Same Configuration** - Load your `imposters.json` without changes
- **Same Behavior** - Predicates, responses, behaviors all work identically

### Blazing Fast Performance

Both engines, same machine, same load — measured on two very different hosts so you can see how
much of the gap is the engine and how much is the hardware:

| Workload | Apple M4 laptop<br><sub>Mountebank → Rift</sub> | AMD EPYC 9V74, 16 vCPU<br><sub>Mountebank → Rift</sub> |
|:---------|:--------------------------|:----------------------------|
| Simple static stub | 8,898 → 214,818 RPS (**24x**) | 5,982 → 324,952 RPS (**54x**) |
| Deep path match (410 stubs) | 1,344 → 209,523 RPS (**156x**) | 542 → 322,530 RPS (**595x**) |
| Complex AND/OR predicates | 4,703 → 191,987 RPS (**41x**) | 1,814 → 259,548 RPS (**143x**) |
| JSON body equals | 7,611 → 199,670 RPS (**26x**) | 2,730 → 294,294 RPS (**108x**) |
| JSONPath predicate | 4,312 → 199,404 RPS (**46x**) | 1,921 → 304,796 RPS (**159x**) |
| XPath predicate | 5,542 → 187,869 RPS (**34x**) | 1,966 → 247,897 RPS (**126x**) |
| Regex path (100 patterns) | 112 → 207,024 RPS (**1,857x**) | 52 → 317,851 RPS (**6,160x**) |

Read the two columns together, not separately. Rift gets **faster** with more cores (215k → 325k);
Mountebank gets **slower** (8,898 → 5,982), because it is single-threaded and the server's
individual cores are slower than the laptop's. So the EPYC multipliers are inflated at both ends —
the M4 column is the more conservative read, and it is still 24x–1,857x.

<sub>Measured 2026-07-20 — Rift built from `master` (`924cf73`) vs Mountebank `2.9.1`, native
processes (no Docker), `oha` at 50 keep-alive connections, 20s/scenario after warmup, each engine
run alone on the same machine. Each figure is the median of 3 repetitions; per-scenario spread was
≤12% on the M4 (a laptop thermally throttles over a 30-minute run — both engines lost ~7% between
the first and last repetition) and ≤5% on EPYC. Throughput scales with matching complexity: Rift
stays flat while Mountebank's per-request cost grows with stub count and predicate type. Full
methodology and all 13 scenarios: [`tests/benchmark`](tests/benchmark/). Your numbers will vary
with hardware and config.</sub>

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
brew tap achird-labs/rift
brew install rift
```

### Cargo (crates.io)

```bash
cargo install rift-http-proxy
```

### Download Binary

Download pre-built binaries from [GitHub Releases](https://github.com/achird-labs/rift/releases):

```bash
# Example for Linux x86_64
curl -LO https://github.com/achird-labs/rift/releases/latest/download/rift-vX.X.X-x86_64-unknown-linux-gnu.tar.gz
tar -xzf rift-vX.X.X-x86_64-unknown-linux-gnu.tar.gz
sudo mv rift-vX.X.X-x86_64-unknown-linux-gnu/bin/* /usr/local/bin/
```

Available platforms:
- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc`

### Build from Source

```bash
git clone https://github.com/achird-labs/rift.git
cd rift
cargo build --release
./target/release/rift-http-proxy
```

### Node.js / npm

For Node.js projects, use the official [rift-node](https://github.com/achird-labs/rift-node) SDK
(`@rift-vs/rift` on npm). It runs the engine three ways — embedded in-process (FFI via the
companion `@rift-vs/rift-embedded` package, no Docker), connected to any running admin endpoint, or
as a managed spawned binary — with a fluent DSL for stubs/predicates/responses/scenarios, plus
Vitest and Jest testkits. Requires Node.js >= 20, ESM-only, zero runtime dependencies:

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

Already on Mountebank, or migrating from the pre-monorepo `@rift-vs/rift`? The Mountebank-compatible
`create()` stays available as a permanent drop-in, so adopting the typed DSL above is incremental,
not a forced rewrite.

See the [rift-node docs](https://github.com/achird-labs/rift-node) for the full feature surface.

### Java / JVM

For JVM projects, use the official [rift-java](https://github.com/achird-labs/rift-java) SDK.
It runs the engine three ways — embedded in-process (Panama FFM, no Docker), connected to any
running admin endpoint, or as a managed spawned binary — with a fluent DSL plus JUnit 5, Spring,
and Testcontainers integrations. Available on Maven Central under `io.github.achird-labs`:

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
    // point your SUT at users.uri(), then assert:
    users.verify(onGet("/api/users/1"), times(1));
}
```

See the [rift-java docs](https://achird-labs.github.io/rift-java/) for the full feature surface.

### Scala

For Scala 3, use the official [rift-scala](https://github.com/achird-labs/rift-scala) SDK. It is
effect-library-native — ZIO, Cats Effect 3 / FS2, Kyo, or no effect system at all — over the same
four transports (embedded, connect, spawn, container):

```scala
libraryDependencies += "io.github.achird-labs" %% "rift-scala-zio" % "0.1.0" % Test
```

```scala
import rift.dsl.*
import rift.zio.Rift

for
  users <- Rift.create(
             imposter("users").record.stub(
               get("/api/users/1").reply(ok.json("""{"id":1}"""))
             )
           )
  _     <- callSut(users.uri)                 // point your SUT at users.uri
  _     <- users.verify(get("/api/users/1"), 1)
yield ()
// .provideShared(Rift.embedded)  — or Rift.connect(uri) / Rift.spawn() / Rift.container()
```

It also ships a [zio-bdd](https://github.com/EtaCassiopeia/zio-bdd) `MockControl` adapter, certified
against zio-bdd's published conformance catalogue. See the
[rift-scala docs](https://achird-labs.github.io/rift-scala/).

---

## Documentation

### Getting Started
- [Installation](https://achird-labs.github.io/rift/getting-started/) - Docker, binary, build from source
- [Quick Start](https://achird-labs.github.io/rift/getting-started/quickstart) - Create your first imposter
- [Node.js Integration](https://achird-labs.github.io/rift/getting-started/nodejs/) - npm package for Node.js
- [Java / JVM SDK](https://github.com/achird-labs/rift-java) - rift-java for JUnit 5, Spring, and Testcontainers
- [Scala SDK](https://github.com/achird-labs/rift-scala) - rift-scala for ZIO, Cats Effect, FS2, and zio-bdd
- [Migration Guide](https://achird-labs.github.io/rift/getting-started/migration) - Using Rift with Mountebank configs

### Mountebank Compatibility
- [Imposters](https://achird-labs.github.io/rift/mountebank/imposters) - Mock server configuration
- [Predicates](https://achird-labs.github.io/rift/mountebank/predicates) - Request matching
- [Responses](https://achird-labs.github.io/rift/mountebank/responses) - Response configuration
- [Behaviors](https://achird-labs.github.io/rift/mountebank/behaviors) - wait, decorate, copy
- [Proxy Mode](https://achird-labs.github.io/rift/mountebank/proxy) - Record and replay

### Configuration
- [Mountebank Format](https://achird-labs.github.io/rift/configuration/mountebank) - JSON configuration
- [Native Rift Format](https://achird-labs.github.io/rift/configuration/native) - YAML for advanced features
- [CLI Reference](https://achird-labs.github.io/rift/configuration/cli) - Command-line options

### Features
- [Fault Injection](https://achird-labs.github.io/rift/features/fault-injection) - Chaos engineering
- [Scripting](https://achird-labs.github.io/rift/features/scripting) - Rhai, JavaScript
- [TLS/HTTPS](https://achird-labs.github.io/rift/features/tls) - Secure connections
- [Metrics](https://achird-labs.github.io/rift/features/metrics) - Prometheus integration
- [TUI](https://achird-labs.github.io/rift/features/tui) - Interactive terminal interface

### Deployment
- [Docker](https://achird-labs.github.io/rift/deployment/docker) - Container deployment
- [Kubernetes](https://achird-labs.github.io/rift/deployment/kubernetes) - K8s patterns

### Reference
- [REST API](https://achird-labs.github.io/rift/api/) - Admin API reference
- [Performance](https://achird-labs.github.io/rift/performance/) - Benchmarks

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

## Used By

Rift powers HTTP mocking in the following projects:

- **[zio-bdd](https://github.com/EtaCassiopeia/zio-bdd)** — a Gherkin-style BDD testing framework for ZIO
- **[zio-openfeature](https://github.com/EtaCassiopeia/zio-openfeature)** — a ZIO-native wrapper around the OpenFeature Java SDK

Using Rift somewhere? Open a PR to add it here.

---

## Contributing

Contributions welcome! Please read our contributing guidelines and submit PRs.

---

## License

Apache License 2.0 - see [LICENSE](LICENSE) for details.

---

## Acknowledgments

- [Mountebank](http://www.mbtest.org/) - The original service virtualization tool that inspired Rift's API and configuration format
