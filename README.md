<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/logo-dark.png">
    <img src="docs/assets/logo.png" width="150" alt="Rift">
  </picture>
</h1>

[![CI](https://github.com/achird-labs/rift/actions/workflows/ci.yml/badge.svg)](https://github.com/achird-labs/rift/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/achird-labs/rift)](https://github.com/achird-labs/rift/releases/latest)
[![crates.io](https://img.shields.io/crates/v/rift-http-proxy)](https://crates.io/crates/rift-http-proxy)
[![Docker](https://img.shields.io/docker/v/zainalpour/rift-proxy?label=docker)](https://hub.docker.com/r/zainalpour/rift-proxy)
[![Docs](https://img.shields.io/badge/docs-achird--labs.github.io%2Frift-blue)](https://achird-labs.github.io/rift/)

A [Mountebank](https://www.mbtest.dev/)-compatible HTTP/HTTPS mock server written in Rust. It loads your existing
`imposters.json` unchanged, keeps its throughput flat as the stub file grows, and embeds in-process in Java, Node,
Go and Scala.

<p align="center">
  <img src="docs/assets/hero.svg" width="100%" alt="A request reaches an imposter, is checked against its stubs in order, and the first match answers from its response cycle; a proxy stub records an upstream answer into a new stub that later requests replay.">
</p>

**[Documentation](https://achird-labs.github.io/rift/)**:
[getting started](https://achird-labs.github.io/rift/getting-started/),
[features](https://achird-labs.github.io/rift/features/),
[Mountebank compatibility](https://achird-labs.github.io/rift/mountebank/),
[SDKs](https://achird-labs.github.io/rift/sdk/),
[deployment](https://achird-labs.github.io/rift/deployment/),
[performance](https://achird-labs.github.io/rift/performance/).

```sh
docker run -p 2525:2525 -p 4545:4545 zainalpour/rift-proxy:latest

curl -X POST http://localhost:2525/imposters -H 'Content-Type: application/json' -d '{
  "port": 4545, "protocol": "http",
  "stubs": [{ "predicates": [{ "equals": { "path": "/hello" } }],
              "responses":  [{ "is": { "statusCode": 200, "body": "Hello, World!" } }] }]
}'

curl http://localhost:4545/hello   # Hello, World!
```

Also on Homebrew (`brew install achird-labs/rift/rift`), crates.io (`cargo install rift-http-proxy`) and
[GitHub Releases](https://github.com/achird-labs/rift/releases); SDKs for
[Java](https://achird-labs.github.io/rift/sdk/java/), [Node](https://achird-labs.github.io/rift/sdk/node/),
[Go](https://achird-labs.github.io/rift/sdk/go/) and [Scala](https://achird-labs.github.io/rift/sdk/scala/).

Rift is compatible with **Mountebank 2.9.5**: every engine change is checked against it by a [differential harness](conformance/differential/)
that sends both the same requests and fails on any difference that is not
[documented](https://achird-labs.github.io/rift/mountebank/differences/). Moving from the first stub of a 310-stub
imposter to the last costs Rift under 1% of its throughput; Mountebank loses 84%
([benchmarks](https://achird-labs.github.io/rift/performance/)).

Used by [zio-bdd](https://github.com/EtaCassiopeia/zio-bdd) and
[zio-openfeature](https://github.com/EtaCassiopeia/zio-openfeature).
[Contributing](CONTRIBUTING.md) · Apache 2.0.
