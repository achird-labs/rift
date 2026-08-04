---
layout: default
title: Getting Started
nav_order: 2
has_children: true
permalink: /getting-started/
---

# Getting Started with Rift

Rift is a high-performance, Mountebank-compatible HTTP/HTTPS mock server. This guide will help you install Rift and create your first imposter.

---

## Installation

### Docker (Recommended)

The easiest way to run Rift is using Docker:

```bash
# Pull the latest image
docker pull zainalpour/rift-proxy:latest

# Run Rift on port 2525 (Mountebank-compatible admin port)
docker run -p 2525:2525 zainalpour/rift-proxy:latest
```

### Homebrew (macOS/Linux)

```bash
brew tap achird-labs/rift
brew install rift
```

This installs four binaries: `rift` (the server), `rift-lint`, `rift-tui`, and `rift-verify`.

### Download Binary

Release archives are published per platform on the
[releases page](https://github.com/achird-labs/rift/releases), named
`rift-vX.Y.Z-<target>.tar.gz` (`.zip` on Windows). Each unpacks to a `bin/` directory
containing `rift`, `rift-lint`, `rift-tui`, and `rift-verify`.

```bash
# macOS (Apple Silicon) — substitute your platform triple from the list below
VERSION=v0.17.0
TARGET=aarch64-apple-darwin

curl -LO https://github.com/achird-labs/rift/releases/download/$VERSION/rift-$VERSION-$TARGET.tar.gz
tar -xzf rift-$VERSION-$TARGET.tar.gz
sudo mv rift-$VERSION-$TARGET/bin/* /usr/local/bin/

rift --version
```

Available platform triples:

- Linux: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
- macOS: `x86_64-apple-darwin`, `aarch64-apple-darwin`
- Windows: `x86_64-pc-windows-msvc`

### Cargo (crates.io)

```bash
cargo install rift-http-proxy
```

Note that cargo names the installed binary after the crate — `rift-http-proxy`, not `rift`. Every
other install method above puts it on your `PATH` as `rift`, which is the name used throughout
these docs.

### Build from Source

Requires Rust 1.92+ (see `rust-version` in `Cargo.toml`):

```bash
git clone https://github.com/achird-labs/rift.git
cd rift
cargo build --release
./target/release/rift-http-proxy
```

### Node.js / npm

For Node.js projects, install the official npm package:

```bash
npm install @rift-vs/rift
```

Usage:

```javascript
import { rift, imposter, onGet, okJson, times } from '@rift-vs/rift';

await using engine = await rift.embedded(); // or rift.connect(url) / rift.spawn()

const users = await engine.create(
  imposter('users').stub(onGet('/api/users/1').willReturn(okJson({ id: 1, name: 'Alice' }))));

await users.verify(onGet('/api/users/1'), times(1));
```

The Mountebank-compatible `rift.create({ port })` API stays available as a permanent drop-in if you
are migrating.

See the [Node.js Integration Guide]({{ site.baseurl }}/getting-started/nodejs/) for complete documentation.

### Java / JVM

For JVM projects, add the official [rift-java](https://github.com/achird-labs/rift-java) SDK to your
test scope:

```xml
<dependency>
  <groupId>io.github.achird-labs</groupId>
  <artifactId>rift-java-core</artifactId>
  <scope>test</scope>
</dependency>
```

Usage:

```java
try (Rift rift = Rift.embedded()) {                  // or Rift.connect(uri) / Rift.spawn()
    Imposter users = rift.create(
        imposter("users").stub(onGet("/api/users/1").willReturn(okJson("{\"id\":1}"))));
    users.verify(onGet("/api/users/1"), times(1));
}
```

`Rift.embedded()` runs the engine in-process over Panama FFM, so no separate binary or container is
needed; `Rift.spawn()` manages a downloaded binary for you, and `Rift.connect(uri)` targets any
running admin endpoint. See the
[rift-java documentation](https://achird-labs.github.io/rift-java/) for the JUnit 5, Spring, and
Testcontainers integrations.

### Go

For Go projects, add the official [rift-go](https://github.com/achird-labs/rift-go) SDK and fetch
the native library once:

```bash
go get github.com/achird-labs/rift-go
go run github.com/achird-labs/rift-go/cmd/rift-fetch@latest -version v0.17.0
```

Usage:

```go
func TestUserLookup(t *testing.T) {
    users := rifttest.Imposter(t, rift.NewImposter("users").
        Stub(rift.OnGet("/api/users/1").Return(rift.OKJSON(`{"id":1}`))))

    callSUT(t, users.BaseURL())

    rifttest.AssertReceived(t, users, rift.OnGet("/api/users/1"), rift.Once())
}
```

The engine runs in-process through [purego](https://github.com/ebitengine/purego) rather than cgo,
so `CGO_ENABLED=0` keeps working and no C toolchain is needed. `rift.Spawn(ctx, …)` manages a
binary instead, and `rift.Connect(url, …)` targets any running admin endpoint — neither needs the
native library. See the [rift-go documentation](https://achird-labs.github.io/rift-go/).

### Scala 3

```scala
libraryDependencies += "io.github.achird-labs" %% "rift-scala-zio" % "0.1.4" % Test
```

There is a module per effect system — ZIO, Cats Effect 3 / FS2, or no effect system at all — over
one shared typed model. See [rift-scala]({{ site.baseurl }}/sdk/scala/).

### All four SDKs

Install snippets, hello-worlds, the transport matrix and the version-compatibility table for Java,
Scala, Node/TypeScript and Go live in [Language SDKs]({{ site.baseurl }}/sdk/).

---

## Verify Installation

Once Rift is running, verify it's working:

```bash
# Check the admin API
curl http://localhost:2525/

# Expected response (hrefs are absolute, built from the admin host and port):
{
  "_links": {
    "config": { "href": "http://localhost:2525/config" },
    "imposters": { "href": "http://localhost:2525/imposters" },
    "logs": { "href": "http://localhost:2525/logs" }
  }
}
```

---

## Your First Imposter

Create a simple HTTP mock that responds to GET requests:

```bash
curl -X POST http://localhost:2525/imposters \
  -H "Content-Type: application/json" \
  -d '{
    "port": 4545,
    "protocol": "http",
    "name": "My First Imposter",
    "stubs": [{
      "predicates": [{
        "equals": {
          "method": "GET",
          "path": "/api/greeting"
        }
      }],
      "responses": [{
        "is": {
          "statusCode": 200,
          "headers": { "Content-Type": "application/json" },
          "body": { "message": "Hello from Rift!" }
        }
      }]
    }]
  }'
```

Test your imposter:

```bash
curl http://localhost:4545/api/greeting

# Response:
{"message":"Hello from Rift!"}
```

---

## Load Existing Configuration

If you have an existing Mountebank configuration file, load it directly:

```bash
# Using Docker
docker run -p 2525:2525 -v $(pwd)/imposters.json:/imposters.json \
  zainalpour/rift-proxy:latest --configfile /imposters.json

# Using binary
./rift --configfile imposters.json
```

Example `imposters.json`:

```json
{
  "imposters": [
    {
      "port": 4545,
      "protocol": "http",
      "stubs": [
        {
          "predicates": [{ "equals": { "path": "/users" } }],
          "responses": [{ "is": { "statusCode": 200, "body": "[]" } }]
        }
      ]
    }
  ]
}
```

---

## Next Steps

- [Quick Start Tutorial]({{ site.baseurl }}/getting-started/quickstart/) - Detailed walkthrough
- [Node.js Integration]({{ site.baseurl }}/getting-started/nodejs/) - npm package for Node.js projects
- [Language SDKs]({{ site.baseurl }}/sdk/) - Java, Scala, Node/TypeScript and Go, with the transport and version-compatibility matrices
- [Java / JVM SDK]({{ site.baseurl }}/sdk/java/) - rift-java for JUnit 5, Spring, and Testcontainers
- [Scala SDK]({{ site.baseurl }}/sdk/scala/) - rift-scala for ZIO, Cats Effect, FS2, and zio-bdd
- [Go SDK]({{ site.baseurl }}/sdk/go/) - rift-go for `testing.T`, embedded via purego (no cgo)
- [Predicates Guide]({{ site.baseurl }}/mountebank/predicates/) - Request matching
- [Responses Guide]({{ site.baseurl }}/mountebank/responses/) - Response configuration
- [Migration Guide]({{ site.baseurl }}/getting-started/migration/) - Switching from Mountebank
