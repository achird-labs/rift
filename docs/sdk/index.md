---
layout: default
title: Language SDKs
nav_order: 10.5
has_children: true
permalink: /sdk/
---

# Language SDKs

Rift has four official SDKs. Each one wraps the same engine — the typed wire model, the three
transports, and the natives distribution — behind an idiomatic DSL and test-framework glue, so you
never hand-write Mountebank JSON or an FFI bridge.

| SDK | Package | Latest | Docs |
|---|---|---|---|
| [Java / JVM]({{ site.baseurl }}/sdk/java/) | `io.github.achird-labs:rift-java-core` | `v0.2.3` | [rift-java](https://achird-labs.github.io/rift-java/) |
| [Scala 3]({{ site.baseurl }}/sdk/scala/) | `io.github.achird-labs::rift-scala-*` | `v0.1.4` | [rift-scala](https://achird-labs.github.io/rift-scala/) |
| [Node / TypeScript]({{ site.baseurl }}/sdk/node/) | `@rift-vs/rift` | `v0.15.1` | [rift-node](https://achird-labs.github.io/rift-node/) |
| [Go]({{ site.baseurl }}/sdk/go/) | `github.com/achird-labs/rift-go` | `v0.2.0` | [rift-go](https://achird-labs.github.io/rift-go/) |

---

## Transports

Every SDK offers these three ways to run the engine, and the full feature surface is available on
each — the transport is a deployment choice, not a capability tier. Some SDKs add more on top
(rift-java's Testcontainers module, rift-scala's `Rift.container()`); see each SDK's own docs.

- **Embedded** — the engine runs in-process, loaded from a native library. No container, no port
  juggling, no separate lifecycle. Fastest to start and the usual choice for unit tests.
- **Connect** — the SDK talks to any already-running admin endpoint over HTTP. Use it against a
  shared or containerised Rift.
- **Spawn** — the SDK downloads and manages the engine binary as a child process. No native-library
  loading, so it works on runtimes where the embedded path is unavailable.

| SDK | Embedded | Connect | Spawn |
|---|---|---|---|
| Java | Panama FFM — JDK 22+ (`rift-java-embedded`), or JDK 21 with `rift-java-embedded-jdk21` | JDK 17+ | JDK 17+ |
| Scala | via the rift-java bridge — JDK 22+ (JDK 21 via the jdk21 artifact) | JDK 21+ | JDK 21+ |
| Node | [koffi](https://koffi.dev/) — Node 20+ | Node 20+ | Node 20+ |
| Go | [purego](https://github.com/ebitengine/purego) — Go 1.24+, `CGO_ENABLED=0` keeps working | Go 1.24+ | Go 1.24+ |

Only the embedded transport needs the platform `librift_ffi` library; connect and spawn do not.
Getting it is a one-line step rather than something you manage by hand — rift-java and rift-scala
bundle it in a natives artifact, rift-node puts it behind the `@rift-vs/rift-embedded` package, and
rift-go ships a `rift-fetch` command that downloads and SHA-256-verifies it.

---

## Version compatibility

| SDK | SDK version | Engine floor |
|---|---|---|
| rift-java | `v0.2.3` | `v0.17.0` |
| rift-scala | `v0.1.4` | `v0.17.0` |
| rift-node | `v0.15.1` | `v0.17.0` |
| rift-go | `v0.2.0` | `v0.17.0` |

**How this table stays honest.** Each SDK pins an engine version and has its own bump automation
that opens a PR when a new engine release lands, so an SDK release is never more than one engine
release behind for long. The table names each SDK's *floor* — the engine it is tested against —
and a newer engine is expected to work. That expectation is not taken on trust: the
[cross-SDK matrix](https://github.com/achird-labs/rift/blob/master/.github/workflows/sdk-matrix.yml)
replays every SDK's conformance lane against the newest engine release daily and on every publish,
so drift shows up as a tracking issue rather than as a user's broken build.

---

## Why the DSLs agree

Every SDK replays the same [SDK conformance corpus]({{ site.baseurl }}/embedding/sdk-conformance/)
in its own CI — a fixture the typed DSL cannot express is a red build. That is what keeps four
independently-written DSLs in lockstep with one engine grammar, and with each other.
