---
layout: default
title: Rift vs WireMock
parent: Comparisons
nav_order: 1
---

# Rift vs WireMock

WireMock is the most widely used mock server on the JVM, and for most teams evaluating Rift it
is the incumbent. This page is the honest version of that comparison: where Rift is genuinely
better, where WireMock is genuinely better, and where you should not switch.

{: .highlight }
> **The one-line version.** WireMock is a JVM library that grew a server. Rift is a server that
> embeds as a library in four languages. Almost every difference below follows from that.

---

## Summary

| | WireMock | Rift |
|:--|:--|:--|
| Implementation | Java / JVM (Jetty) | Rust |
| Maturity | Mature, ~a decade, company-backed | **Beta** (v0.16.x), single maintainer |
| Ecosystem | Large — extensions, Cloud, Spring, a decade of answers | Small |
| Throughput at 1 stub | 83,048 RPS | 334,025 RPS |
| Throughput at 310 stubs | 24,264 RPS (**−71%**) | 326,779 RPS (**−2%**) |
| p99 at 310 stubs | 31.6 ms | 2.5 ms |
| In-process embedding | JVM only | Java, Node, Go, Scala 3 |
| Config format | WireMock mappings JSON / Java DSL | Mountebank `imposters.json` + native YAML |
| Response templating | Handlebars, mature | Date templates, `decorate`, Rhai/JS scripting |
| Recording & playback | Yes, with UI | Yes (proxy mode), no UI |
| OpenAPI import / validation | Cloud | No |
| gRPC / GraphQL / WebSocket | Via extensions | No |
| Licence | Apache-2.0 | Apache-2.0 |

<sub>Performance figures: Intel Xeon Platinum 8573C, 16 vCPU, 2026-07-27. WireMock 3.9.1 on
Temurin 21. `oha` at **256 keep-alive connections**. Full methodology below.</sub>

---

## Where Rift is genuinely better

### 1. Throughput does not degrade as your stub count grows

This is the difference that matters, and it is architectural rather than incidental.

| Scenario | WireMock | Rift | Ratio |
|:---------|---------:|-----:|------:|
| Simple static stub | 83,048 | 334,025 | 4.0x |
| API first stub | 78,906 | 326,778 | 4.1x |
| API middle stub | 42,668 | 326,601 | 7.7x |
| Deep path match (310 stubs) | 24,264 | 326,779 | 13.5x |
| No match | 24,589 | 345,114 | 14.0x |
| Regex path (100 patterns) | 48,982 | 311,815 | 6.4x |
| Complex AND/OR predicates | 56,541 | 251,170 | 4.4x |
| JSON body equals | 63,814 | 314,939 | 4.9x |
| JSONPath predicate | 63,995 | 310,350 | 4.8x |
| XPath predicate | 46,408 | 231,868 | 5.0x |
| Response templating | 48,541 | 260,762 | 5.4x |
| Header match (last of 100) | 24,539 | 180,255 | 7.3x |
| Query match (last of 100) | 20,529 | 190,867 | 9.3x |

Read the two engines down their own columns rather than across:

- WireMock: **83,048 → 24,264** from a trivial stub to a 310-stub deep path match. It loses 71%.
- Rift: **334,025 → 326,779** over the same pair. It loses 2%.

WireMock pays per candidate stub, so per-request cost tracks the size of your mappings. Rift's
matcher cost is small relative to its fixed per-request overhead, so adding stubs barely moves
it.

That shape, not the headline multiple, is the argument. Nobody plans to have 400 stubs — you
have twelve, then an integration lands, then someone mocks an error path, then a migration adds
forty that outlive it. The person who adds the stub pays nothing; the cost lands on whoever
profiles the suite two years later.

The same effect shows up as latency, which is what you actually feel per test: **p99 of 31.6 ms
vs 2.5 ms** on the 310-stub scenario.

### 2. One engine, four languages, in-process

WireMock's in-process story is excellent — on the JVM. `WireMockServer` in a JUnit test is
fast, well-integrated, and hard to beat if the JVM is all you run.

Rift's engine is native code behind a C ABI, so the *same binary* embeds in-process in four
ecosystems:

| Language | Mechanism | Testing integration |
|:--|:--|:--|
| Java / JVM | Panama FFM | JUnit 5, Spring, Testcontainers |
| Node / TypeScript | koffi | Vitest, Jest testkits |
| Go | purego (**no cgo**) | `testing.T` helpers |
| Scala 3 | via rift-java | ZIO, Cats Effect 3 / FS2, pure |

A JVM library cannot become a Go library. If your organisation runs more than one language,
the practical outcome today is several mocking tools with incompatible fixtures — and a
contract change means several teams update several unrelated things, and one of them forgets.

Each SDK also runs the engine four ways behind the same DSL: **embedded** (in-process),
**connect** (any running admin endpoint), **spawn** (managed binary), **container**. Switching
between them is a constructor argument, not a rewrite — so the mock in a unit test and the mock
in a shared environment are the same engine with the same semantics.

See [Embedding & SPI]({{ site.baseurl }}/embedding/).

### 3. Mocking dependencies you cannot repoint

Rift can sit in the request path as a **TLS-terminating forward proxy**: it terminates the
HTTPS call, matches it with the ordinary predicate engine, and serves inline or forwards to an
imposter. That covers the vendor SDK with a compiled-in `https://cdn.vendor.com/config.json`
and no base-URL setter — the dependency most teams quietly stop integration-testing.

There is also a **front door**: one listener serving many imposters, routed by the `Host` header
the client believes it is calling, so an unmodified system under test needs no configuration
at all.

WireMock has a browser proxy mode that overlaps with the first of these. Rift's version is more
complete and is a first-class matching path rather than an inspection aid.

It is a man-in-the-middle proxy. The CA is generated per run, trust should be process-scoped,
and it belongs in test environments — see [Intercept Proxy]({{ site.baseurl }}/features/intercept-proxy/)
and [Front Door]({{ site.baseurl }}/features/front-door/).

### 4. Parallel test isolation without per-shard instances

**Spaces** partition one imposter's stubs, scenario state and recorded requests by a correlation
id, so parallel CI shards share a port without seeing each other's stubs. **Flow state** gives
stateful mocks (retry-then-succeed, counters, saga progress) a per-flow key/value store instead
of global state.

The usual alternatives — one instance per shard with port allocation, or resetting between tests
and losing the parallelism — both cost something real. See
[Spaces]({{ site.baseurl }}/features/spaces/) and [Flow State]({{ site.baseurl }}/features/flow-state/).

### 5. Operational and authoring tooling

- **[Debug mode]({{ site.baseurl }}/features/debug-mode/)** — ask *why* a request matched or
  did not, instead of executing the response.
- **[Stub analysis]({{ site.baseurl }}/features/stub-analysis/)** — static detection of stubs
  shadowed or made unreachable by an earlier one.
- **[`rift-lint`]({{ site.baseurl }}/features/linting/)** — validate config in CI before it loads.
- **`rift-verify`** — generate requests from your predicates to test your own stubs.
- **[`rift-tui`]({{ site.baseurl }}/features/tui/)** — interactive terminal UI over imposters and stubs.
- **[Prometheus metrics]({{ site.baseurl }}/features/metrics/)** — request counts, latency
  histograms, fault-injection stats on `:9090/metrics`.
- **[Hot reload]({{ site.baseurl }}/features/hot-reload/)** — `POST /admin/reload` diffs the
  running imposters against the new config and touches only what changed.
- **Native multi-arch binaries** — Linux gnu/musl on x86_64 and aarch64, macOS Intel and Apple
  Silicon, Windows. No JVM on the box.

---

## Where WireMock is genuinely better

Stated plainly, because you should not find these out after migrating.

### Maturity and ecosystem

WireMock has been in production use for roughly a decade, has a company behind it, and has an
extension ecosystem, a Kotlin DSL, a Cloud product, deep Spring Boot integration, and years of
accumulated Stack Overflow answers. Rift is **beta**, at v0.16.x, and small. For a lot of teams
that difference outweighs everything in the section above, and that is a reasonable call.

### Commercial support

WireMock Inc. sells support and a hosted product. Rift has GitHub issues.

### Response templating

WireMock's Handlebars templating is mature, well-documented and broadly used. Rift covers this
ground differently — [date templates]({{ site.baseurl }}/features/date-templates/), the
`decorate`/`copy`/`lookup` behaviors, and [Rhai or JavaScript scripting]({{ site.baseurl }}/features/scripting/)
— which is more powerful at the top end and less convenient for the common case of interpolating
a request value into a response body.

### Specification-driven mocking

OpenAPI import, OpenAPI traffic validation, managed dynamic state and data-source-backed
responses are part of WireMock's commercial offering. **Rift has no equivalent today.** If your
workflow starts from a spec, that is a real gap.

### Protocols beyond HTTP

WireMock supports gRPC via an official extension (`wiremock-grpc-extension`, WireMock 3.2.0+)
and documents GraphQL support. Rift is **HTTP/HTTPS only** — no gRPC, no WebSockets, no GraphQL
as a distinct protocol (GraphQL over HTTP is matchable as an HTTP body), no TCP/SMTP/LDAP.

### Matching features Rift does not have

- **JSON-schema matching** — WireMock has it, Rift does not.
- **Explicit stub priority** — WireMock has a `priority` field (1 is highest, unspecified stubs
  default to 5). Rift evaluates stubs in declaration order and takes the first match (Mountebank
  semantics), so you order rather than rank.

### Recording UX and a web UI

Both engines record via proxy, but WireMock's recording workflow and its inspection UI are
considerably more polished. Rift's equivalent is the TUI and the recorded-requests API; there
is no web UI for stub authoring.

---

## When you should not switch

Use WireMock if any of these describe you:

- **You are an all-JVM shop and it is working.** The polyglot argument is Rift's strongest and
  it is worth nothing to you. "Working" beats "faster".
- **Your stub set is small and stable.** At thirty stubs the scan costs you nothing, and the
  entire performance argument on this page is about a problem you do not have. Go count before
  you care.
- **You need the ecosystem** — OpenAPI-driven mocking, gRPC, a web UI, commercial support, or
  the fact that every contractor who walks in the door already knows WireMock.
- **Beta is not acceptable** for the position this sits in for you.

The case where the architectural argument actually bites: large and growing stub sets,
integration suites where per-test latency multiplies out across thousands of tests, and
organisations running more than one language.

---

## Migrating

There is no config compatibility between WireMock and Rift, so this is genuine translation
work, not a drop-in. (If you are coming from **Mountebank**, it *is* a drop-in — see the
[migration guide]({{ site.baseurl }}/getting-started/migration/).)

### Concept mapping

| WireMock | Rift |
|:--|:--|
| Stub mapping | Stub inside an [imposter]({{ site.baseurl }}/mountebank/imposters/) |
| `request` matcher | [`predicates`]({{ site.baseurl }}/mountebank/predicates/) |
| `response` | [`responses`]({{ site.baseurl }}/mountebank/responses/) with `is` |
| `urlPath` / `method` | `equals: { path, method }` |
| `urlPattern` / `urlPathPattern` | `matches: { path }` (regex) |
| `equalToJson` | `equals` / `deepEquals` on `body` |
| `matchesJsonPath` | `jsonpath` |
| `matchesXPath` | `xpath` |
| `containing` / `equalTo` / `matching` | `contains` / `equals` / `matches` |
| `equalToIgnoreCase` | `equals` with `caseSensitive: false` |
| `absent` | `not` + `exists` |
| `notContaining` / `notMatching` | `not` + `contains` / `matches` |
| `matchesJsonSchema` | **No equivalent** |
| `priority` | Declaration order — see below |
| `scenarioName` | `scenarioName` |
| `whenScenarioStateIs` | `requiredScenarioState` |
| `willSetStateTo` | `newScenarioState` |
| `fixedDelay` / `randomDelay` | `wait` behavior, [fault injection]({{ site.baseurl }}/features/fault-injection/) |
| Fault kinds | Direct equivalents — see below |
| `proxyBaseUrl` | `proxy` response |
| Record & playback | [Proxy mode]({{ site.baseurl }}/mountebank/proxy/) |
| `verify(...)` | `verify` in the SDKs, or the recorded-requests API |
| Java DSL | The [Java]({{ site.baseurl }}/getting-started/), Node, Go or Scala SDK DSL |

Rift's predicate set is `equals`, `deepEquals`, `contains`, `startsWith`, `endsWith`, `matches`,
`exists`, `jsonpath`, `xpath`, and `and` / `or` / `not`, with a `caseSensitive` option.

### Two things that translate one-for-one

**Scenario state machines.** Rift's stub-level `requiredScenarioState` and `newScenarioState`
are the direct equivalents of WireMock's `whenScenarioStateIs` and `willSetStateTo`, keyed by
`(flow_id, scenarioName)`. A WireMock scenario translates field-for-field. The `flow_id` part is
an addition, not a difference — it is what lets the same state machine run concurrently in
parallel test shards without collisions (see [Spaces]({{ site.baseurl }}/features/spaces/)).

**Connection faults.** Rift's `_rift.fault.tcp` accepts WireMock's fault names verbatim:

| WireMock `Fault` | Rift value | Rift short alias |
|:--|:--|:--|
| `CONNECTION_RESET_BY_PEER` | `CONNECTION_RESET_BY_PEER` | `reset` |
| `EMPTY_RESPONSE` | `EMPTY_RESPONSE` | `empty` |
| `RANDOM_DATA_THEN_CLOSE` | `RANDOM_DATA_THEN_CLOSE` | `garbage` / `random` |
| `MALFORMED_RESPONSE_CHUNK` | `MALFORMED_RESPONSE_CHUNK` | `malformed` |

### Three behavioural differences that will bite you

1. **Unmatched requests.** WireMock returns **404** with a near-miss report. Rift's default for
   a request that matches no stub is a **200**. If your tests assert on 404s for unmatched
   traffic, add an explicit catch-all stub.
2. **No OR across different fields in one WireMock stub.** WireMock cannot express an OR across
   two *different* headers within a single mapping, so a config written around that limitation
   often expands into more mappings than it needs. Rift's `or` handles it directly, and
   translating usually *reduces* your stub count. (This is also why the
   `complex_predicate` benchmark row is not strictly like-for-like — see the caveats below.)
3. **Stub ordering is the priority mechanism, and the default tie-break is inverted.** WireMock
   resolves by `priority` (1 highest, default 5) and otherwise uses the **most recently added**
   matching stub. Rift evaluates in declaration order and takes the **first** match. So a
   faithful translation sorts by priority ascending and then *reverses* same-priority stubs —
   getting this backwards silently changes which stub answers. Put more specific stubs first.

There is no automated importer today — translation is manual. One is planned:
[#890](https://github.com/achird-labs/rift/issues/890) covers the mechanical translation
(matchers, responses, faults, scenarios, delays, `priority` ordering) and
[#891](https://github.com/achird-labs/rift/issues/891) covers response templating. If a large
migration is blocking you, comment on #890 with the shape of your config — that is what decides
priority and what the test corpus gets built from.

---

## Benchmark methodology

The full write-up, including the 50-connection series and the stock-thread-pool column, is in
[Performance]({{ site.baseurl }}/performance/#rift-vs-wiremock).

**Setup.** Intel Xeon Platinum 8573C, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-27. WireMock
3.9.1 on Temurin 21; Rift built from `master`. `oha` at 256 keep-alive connections, 20 s per
scenario after a 10 s warmup — identical settings for both engines, each of which ran alone.
Median of 3 repetitions; spread ≤5.6% for WireMock and ≤1.2% for Rift.

WireMock cannot consume imposter JSON, so its mappings are *generated* from the same fixture the
Mountebank suite uses. That keeps the two suites on the same workload rather than letting them
drift apart.

**Quote the connection count.** At 50 connections the same suite measures **3.1x–8.1x** rather
than 4.0x–14.0x. This is not Rift slowing down: at 50 connections Rift is bounded by the
closed-loop harness (`connections ÷ latency`) rather than by its own capacity, while WireMock is
engine-bound at both points. Any Rift-vs-WireMock number without its connection count attached
is meaningless.

### What we did to keep it fair

**WireMock's thread pool is pinned to `max(cores, connections)`.** Its default is 10 threads,
which bounds in-flight requests at 10 — far below the 256 offered here. Benchmarking against
that default would measure the pool, not the engine. The pin is a fairness guarantee, not a
speedup: the stock 10-thread column is published beside it and lands within noise on this
hardware (86,840 vs 83,048 on a simple stub), because the CPU saturates before the pool binds.

**WireMock's request journal is off** (`--no-request-journal`). It records every request
unbounded by default, and Rift and Mountebank are both measured with recording off. In our
measurements that one flag is worth roughly 30% throughput and 5x p99.9 **to WireMock** — so
leaving it on would have flattered Rift.

### Two caveats we do not bury

**`complex_predicate` is not like-for-like.** WireMock cannot express an OR across two different
headers in one stub, so that imposter's 50 stubs become 101 mappings, and the measured request
matches the 50th candidate where Rift matches the 25th — roughly twice the scan. That is a
genuine cost of modelling the workload in WireMock, but it is not an apples-to-apples predicate
comparison, and the 4.4x on that row should be read with it in mind.

**The all-2xx sanity gate is weaker for WireMock.** WireMock 404s an unmatched request, so the
suite installs a catch-all empty-200 to reproduce Rift's no-match default — which means an
all-2xx status distribution no longer proves that anything matched. The per-scenario body-marker
assertion is the check that actually catches a mistranslated stub.

### Reproduce it

```bash
gh workflow run benchmark-publish.yml -f connections=256 -f run_sweep=false
```

The harness is in [`tests/benchmark`](https://github.com/achird-labs/rift/tree/master/tests/benchmark)
and runs both engines. If you think something here is unfair, re-run it — and if you find a
configuration that narrows the gap, [open an issue](https://github.com/achird-labs/rift/issues)
and we will publish the corrected number.

---

## A note on WireMock

Rift competes with WireMock on architecture, not on quality. WireMock is a well-engineered
project that has served an enormous number of teams for a long time, and the gap measured on
this page is a consequence of a thread-per-request JVM server with a per-candidate matcher —
which was an entirely sensible design for what it set out to do — rather than of anything done
badly.

WireMock and WireMock Cloud are products of WireMock Inc. Rift is not affiliated with, endorsed
by, or derived from them. All comparative claims here are measured, and the methodology and
harness are published so they can be checked.
