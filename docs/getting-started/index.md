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

### Download Binary

Download pre-built binaries from the [releases page](https://github.com/achird-labs/rift/releases):

```bash
# Linux (x86_64)
curl -L https://github.com/achird-labs/rift/releases/latest/download/rift-http-proxy-linux-x86_64 -o rift
chmod +x rift
./rift

# macOS (Apple Silicon)
curl -L https://github.com/achird-labs/rift/releases/latest/download/rift-http-proxy-darwin-aarch64 -o rift
chmod +x rift
./rift

# macOS (Intel)
curl -L https://github.com/achird-labs/rift/releases/latest/download/rift-http-proxy-darwin-x86_64 -o rift
chmod +x rift
./rift
```

### Build from Source

Requires Rust 1.70+:

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
import rift from '@rift-vs/rift';

const server = await rift.create({ port: 2525 });
// Create imposters, run tests...
await server.close();
```

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

---

## Verify Installation

Once Rift is running, verify it's working:

```bash
# Check the admin API
curl http://localhost:2525/

# Expected response:
{
  "_links": {
    "imposters": { "href": "/imposters" },
    "config": { "href": "/config" },
    "logs": { "href": "/logs" }
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
- [Java / JVM SDK](https://github.com/achird-labs/rift-java) - rift-java for JUnit 5, Spring, and Testcontainers
- [Scala SDK](https://github.com/achird-labs/rift-scala) - rift-scala for ZIO, Cats Effect, FS2, and zio-bdd
- [Predicates Guide]({{ site.baseurl }}/mountebank/predicates/) - Request matching
- [Responses Guide]({{ site.baseurl }}/mountebank/responses/) - Response configuration
- [Migration Guide]({{ site.baseurl }}/getting-started/migration/) - Switching from Mountebank
