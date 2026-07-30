---
layout: default
title: Rift vs Microcks
parent: Comparisons
nav_order: 2
---

# Rift vs Microcks

[Microcks](https://microcks.io/) is the open-source API mocking and testing tool a team usually
reaches for before a commercial one: Apache-2.0, a CNCF **Incubating** project since
[11 May 2026](https://microcks.io/blog/), and free permanently — the governance sits with the CNCF,
so it cannot be relicensed out from under you.

{: .highlight }
> **The one-line version.** Microcks starts from your **specification** and gives you mocking *and*
> contract testing across many protocols. Rift starts from your **stubs** and gives you a fast HTTP
> mock that embeds in-process in four languages. They overlap on HTTP mocking and diverge almost
> everywhere else.

---

## Summary

| | Microcks | Rift |
|:--|:--|:--|
| Implementation | Java / JVM (Spring Boot) | Rust |
| Governance | CNCF Incubating, Apache-2.0 | Apache-2.0, single maintainer |
| Maturity | Mature, CNCF-backed, named enterprise users | **Beta** (v0.16.x) |
| Authoring model | **Spec-driven** — OpenAPI, AsyncAPI, Postman, gRPC, GraphQL, SoapUI | **Stub-driven** — Mountebank `imposters.json` + native YAML |
| Throughput at 2 stubs | 16,192 RPS | 347,604 RPS |
| Throughput at 310 stubs | 6,420 RPS (**−60%**) | 338,404 RPS (**−3%**) |
| p99 at 310 stubs | 238 ms | 2.4 ms |
| Protocols | HTTP, AsyncAPI, Kafka, MQTT, AMQP, WebSocket, gRPC, GraphQL | **HTTP/HTTPS only** |
| Contract testing | Yes — validates a real implementation against the spec | No |
| In-process embedding | No (server, or Testcontainers) | Java, Node, Go, Scala 3 |
| Web UI | Yes | No (TUI + admin API) |
| OpenAPI import / validation | Core feature | No |

<sub>Performance figures: AMD EPYC 7763, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-30.
Microcks 1.14.0 on Temurin 21, native JVM; Rift built from source. `oha` at **256 keep-alive
connections**, 20 s per scenario after a 10 s warmup — identical settings for both engines, each of
which ran alone. Median of 3 repetitions; spread ≤3.3% for Microcks, ≤2.7% for Rift. Full
methodology below.</sub>

---

## HTTP mock serving

Six of the benchmark suite's thirteen scenarios have a faithful Microcks translation. The other
seven are excluded with reasons, [below](#what-is-not-comparable-and-why) — approximating them would
publish a ratio between two different workloads.

| Scenario | Microcks | Rift | Rift/Microcks | Microcks p50 | Rift p50 | Microcks p99 | Rift p99 |
|:---------|---------:|-----:|--------------:|-------------:|---------:|-------------:|---------:|
| Simple static stub | 16,192 | 347,604 | **21.5x** | 8.7 ms | 0.6 ms | 78.9 ms | 2.3 ms |
| API first stub | 6,457 | 338,592 | **52.4x** | 17.2 ms | 0.7 ms | 239.3 ms | 2.4 ms |
| API middle stub | 6,447 | 339,056 | **52.6x** | 17.7 ms | 0.7 ms | 249.5 ms | 2.4 ms |
| API last stub (310 stubs) | 6,420 | 338,404 | **52.7x** | 19.9 ms | 0.7 ms | 238.0 ms | 2.4 ms |
| No match | 6,487 | 354,170 | **54.6x** | 35.1 ms | 0.6 ms | 166.2 ms | 2.3 ms |
| Query match (last of 100) | 14,397 | 195,966 | **13.6x** | 13.1 ms | 1.1 ms | 74.8 ms | 4.0 ms |

Rift is **13.6x–54.6x** Microcks' throughput on the HTTP matching path, and holds a p99 of ~2.4 ms
where Microcks' runs 75–250 ms at this concurrency.

That is a wider gap than [Rift vs WireMock]({{ site.baseurl }}/comparisons/wiremock/) (4.0x–14.0x), and the reason is not that
Microcks is a worse mock server. It is that Microcks is a *different kind of product* — a
spec-driven mocking **and contract-testing** platform with a web UI, multi-tenancy and eight
protocols, built on Spring Boot with a database behind it. Raw HTTP stub-serving throughput is not
what it was optimised for, and for most of its users it is not the binding constraint.

---

## Stub growth: the claim, and what actually happens

Rift's central performance claim is that throughput does not degrade as your stub count grows. This
benchmark existed to find out whether that claim survives against Microcks, and **the honest answer
is that it survives — but the mechanism is different from WireMock's, and the difference matters.**

Read each engine down its own column, not across:

| Point | Workload | Microcks | Rift | WireMock† |
|:------|:---------|---------:|-----:|----------:|
| Simple static stub | 2 stubs | 16,192 | 347,604 | 83,048 |
| API first stub | 1st of 310 | 6,457 | 338,592 | 78,906 |
| API middle stub | ~155th of 310 | 6,447 | 339,056 | 42,668 |
| API last stub | 310th of 310 | 6,420 | 338,404 | 24,264 |
| | **trivial → 310 stubs** | **−60%** | **−3%** | **−71%** |

<sub>† WireMock 3.9.1 at the same settings, from the run published on the
[WireMock page]({{ site.baseurl }}/comparisons/wiremock/) (2026-07-27); cited rather than
re-measured. **Not the same physical host:** GitHub's `ubuntu-16core` pool is heterogeneous, and that
run landed on an Intel Xeon 8573C while this one got an AMD EPYC 7763 — Rift itself reads 334,025 on
the former and 347,604 on the latter, a ~4% difference. So the WireMock column is safe to read
*down* (its own degradation is measured within one run on one host) and **not** safe to compare
across to the other two columns in absolute terms. Microcks and Rift are from one dispatch on
2026-07-30.</sub>

So all three engines are *not* alike, but they fail in two distinct ways:

**WireMock pays per candidate stub.** Its cost tracks position in the mapping list: 78,906 → 24,264
(**−69%**) *within the same 310-stub corpus*, purely by moving the matching stub from first to last.

**Microcks pays per resident corpus, not per candidate.** Its three API points are
6,457 / 6,447 / 6,420 — a **−0.6%** spread across first, middle and last of the same 310 operations,
which is inside the run's own noise. Microcks resolves an operation by path and verb rather than
scanning candidates, so *where* your stub sits costs nothing. What costs is how much is loaded: the
drop happens between the 2-operation service and the 310-operation one, not inside it.

**Rift pays neither** — −3% across the interval and −0.06% by position.

### What this does and does not support

Being precise about this, because the interval in the table is doing two things at once:

- **Supported: Rift's flat-under-stub-growth claim generalises to Microcks.** −3% vs −60% over the
  same interval, at ≤3.3% spread. This is not a marginal result.
- **Supported: for Microcks, stub *ordering* is free.** If you are choosing where to put a new stub
  in a Microcks spec to keep matching fast, the answer is that it does not matter. That is the same
  architectural property Rift has, and WireMock does not.
- **NOT supported: that Microcks' −60% is caused by corpus size alone.** The `simple_health` point is
  a *different service* with a different response (2 bytes of `text/plain` vs ~33 bytes of JSON), so
  that interval mixes corpus size with payload. The clean, confound-free measurement here is the
  **−0.6% position invariance**, not the −60%.
- **NOT supported: any claim about Microcks beyond HTTP.** Nothing here measures AsyncAPI, Kafka,
  gRPC or contract testing, which is most of what Microcks is for.
- **NOT supported: a generalised "competitors pay per candidate stub" story.** WireMock does;
  Microcks does not. Rift's advantage over Microcks on this axis is absolute throughput and tail
  latency, not scan behaviour.
- **NOT supported: a three-way absolute ranking from the table above.** The WireMock column comes
  from a different physical CPU in the same runner pool (see the footnote). Each column's *own*
  degradation is a within-run measurement and is sound; the absolute cross-column gaps between
  WireMock and the other two are not, and a three-way table would need one dispatch to fix that.

A directional, non-publication-grade observation that points at the corpus-size mechanism: on a
10-core laptop, holding the request and service fixed and growing one Microcks service from 310 to
3,100 operations moved it from ~4,200 to ~1,000 RPS, with first-operation and last-operation requests
staying within noise of each other at both sizes. That isolates size from position but was not run
under the published methodology, so treat it as a hypothesis worth its own benchmark rather than a
number.

---

## Where Microcks is genuinely better

Stated plainly, because these are the reasons most teams should pick Microcks.

### Specification-driven mocking is the whole point

You hand Microcks an OpenAPI, AsyncAPI, Postman, gRPC/protobuf, GraphQL or SoapUI artifact and it
derives the mock. Your mock cannot drift from your contract, because the contract *is* the mock.
**Rift has no equivalent** — no OpenAPI import, no spec validation. If your workflow starts from a
spec, that is a decisive difference and this whole page's throughput table is beside the point.

### Contract testing, not just mocking

Microcks' test runner points at a *real* implementation and verifies it conforms to the same spec
that backs the mock, with several validation strategies. That closes the loop between "my mock says
X" and "my service does X". Rift has `rift-verify` (generate requests from your own predicates) and
`rift-lint`, which are useful but are not contract testing against a live implementation.

### Protocols beyond HTTP

AsyncAPI, Kafka, MQTT, AMQP, WebSocket, Google Pub/Sub, gRPC and GraphQL. Rift is **HTTP/HTTPS
only**. If you need to mock an event-driven system, Rift cannot do it at any speed.

### Governance and longevity

CNCF Incubating under Apache-2.0, with named enterprise adopters. Rift is beta, Apache-2.0, and
maintained by one person. For a lot of organisations that difference outweighs everything in the
performance table, and that is a reasonable call.

### A web UI and multi-tenancy

Microcks ships a UI for browsing services, examples, invocation statistics and test results, plus
Keycloak-based auth. Rift's equivalents are a TUI and the admin API.

---

## Where Rift is genuinely better

### Throughput and tail latency on the HTTP path

13.6x–54.6x, and a p99 two orders of magnitude lower. If your integration suite makes a large number
of HTTP calls and per-test latency multiplies out across thousands of tests, that is the argument.

### In-process embedding in four languages

Rift's engine is native code behind a C ABI, so the same binary embeds **in-process** in Java
(Panama FFM), Node (koffi), Go (purego, no cgo) and Scala 3. Microcks is a server — you run it, or
you start a container via Testcontainers. Rift also runs that way (`connect`, `spawn`, `container`),
but the embedded mode has no process boundary at all, and switching between the four is a
constructor argument.

### Mocking dependencies you cannot repoint

Rift can act as a **TLS-terminating forward proxy** and match intercepted HTTPS traffic with the
ordinary predicate engine, plus a **front door** that routes many imposters off the `Host` header.
That covers the vendor SDK with a compiled-in URL and no base-URL setter. Microcks expects you to
point your client at its mock endpoint.

### Parallel test isolation

**Spaces** partition one imposter's stubs, scenario state and recorded requests by correlation id,
and **flow state** gives stateful mocks a per-flow store — so parallel CI shards share a port without
seeing each other's stubs, instead of running one instance per shard.

### Footprint

A native multi-arch binary with no JVM and no database. Microcks needs a JVM and MongoDB (the
`uber` distribution embeds an in-memory store for testing).

---

## When to use which

Use **Microcks** if you start from a specification, need contract testing, need any protocol other
than HTTP, need a UI, or need CNCF-backed governance. That is most teams.

Use **Rift** if you author stubs directly, need in-process embedding across more than one language,
need to intercept traffic you cannot repoint, or are genuinely bound by mock throughput and tail
latency at high stub counts.

They are not mutually exclusive: a spec-driven Microcks instance in a shared environment and an
embedded Rift in unit tests is a coherent combination.

---

## Benchmark methodology

**Setup.** AMD EPYC 7763 64-Core, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-30. Microcks 1.14.0 on
Temurin 21; Rift built from source. `oha` at 256 keep-alive connections, 20 s per scenario after a
10 s warmup — identical for both engines, each of which ran alone on a disjoint port range. Median of
3 repetitions, spread stated.

**Microcks runs as a native JVM, not a container.** A container's virtualised network is not a
property of the engine, and measuring one would have made this column non-comparable with the other
engines in the suite (all native processes). The runnable Spring Boot jar is lifted out of the
official image once, because Microcks publishes only non-repackaged jars to Maven Central.

**"N stubs" had to be translated.** Microcks is spec-driven, so the harness generates an OpenAPI
3.0.2 document per imposter from the *same* fixture the Rift and Mountebank suites use, mapping
"N stubs" to "N `path` × `verb` operations". That is the nearest honest analogue and it is a
translation, not an identity. Response bodies are emitted as raw strings so Microcks' bytes are
byte-identical to Rift's, which keeps the per-scenario body-marker assertion strong rather than
whitespace-insensitive.

### What we did to keep it fair

Every item here except the first moves the number in **Microcks'** favour.

**Invocation statistics are off** (`mocks.enable-invocation-stats=false`) — and this is the one that
runs the other way, so it gets stated first. Microcks defaults it to **on**: it counts every mock
call and persists a per-service/per-day/per-hour record (verified — 500 requests produce
`dailyCount: 500`). It is the direct analogue of WireMock's request journal, which is disabled in
*its* leg, and Rift and Mountebank are both measured with recording off. Leaving it on would have
compared Microcks-with-recording against Rift-without — an error flattering **Rift**, which is the
direction that must never be taken quietly. The CORS policy
(`mocks.rest.enable-cors-policy=false`) goes with it: on by default, four `Access-Control-*` headers
per response that neither Rift nor WireMock emits.

**And here is what those two flags were actually worth: nothing measurable.** The same workload with
Microcks launched exactly as it ships (both defaults on, stock Tomcat pool):

| Scenario | Microcks (tuned) | Microcks (stock defaults) | Difference |
|:---------|-----------------:|--------------------------:|-----------:|
| Simple static stub | 16,192 | 15,619 | −4% |
| API first stub | 6,457 | 6,455 | −0% |
| API middle stub | 6,447 | 6,444 | −0% |
| API last stub | 6,420 | 6,393 | −0% |
| No match | 6,487 | 6,483 | −0% |
| Query match | 14,397 | 14,616 | +2% |

Within noise, scattered both ways. The tuning was the *correct* thing to do for consistency with the
other engines, and it changed nothing — which is exactly the kind of result that should be published
rather than quietly dropped once it stops being dramatic.

**Tomcat's pool is pinned to `max(cores, connections)`.** Its default is 200 while this table drives
256, so the stock configuration would bound in-flight requests below the offered concurrency and
measure the pool rather than the engine. The stock column above uses Tomcat's own default, and lands
within noise here — the CPU saturates before the pool binds.

**Heap is pinned** (`-Xms == -Xmx = 4g`) rather than left to ergonomic sizing, which is a fraction of
host RAM and would silently differ between machines.

**One imposter per JVM.** Microcks multiplexes every service on one port, and its throughput is
sensitive to total resident corpus size, so loading all 14 fixture imposters into one process would
have penalised it against WireMock's per-imposter JVM. Each Microcks instance holds exactly the
imposter under test.

**AsyncAPI is off** (`-Dasync-api.enabled=false`) — background work unrelated to HTTP mocking.
**Logging is at WARN**, because a per-request log site turns a throughput benchmark into a
measurement of the logging pipeline.

### Caveats we do not bury

**Microcks 404s an unmatched request.** Rift's default for a request matching no stub is an empty
200, and the WireMock suite installs a catch-all to reproduce that. Microcks has no catch-all
mechanism, so the `No match` row is measured as the 404 it genuinely returns — a cheaper path than
rendering a response, so that row flatters Microcks.

**The `uber` profile uses an in-memory MongoDB.** A production Microcks talks to a real MongoDB over
a socket, so this configuration is *faster* than a real deployment.

**The first scenario in each group meets a colder JIT.** Scenarios sharing an imposter share one JVM
and run in fixture order. The per-scenario warmup and median-of-3 absorb most of this; it biases
against whichever scenario runs first, not against Microcks overall. WireMock's leg has the same
property.

**Absolute numbers are machine-specific and ratios move with concurrency.** Any Rift-vs-Microcks
number without its connection count attached is meaningless.

### What is not comparable, and why

Seven of the thirteen scenarios are excluded rather than approximated:

| Scenario | Why it is excluded |
|:--|:--|
| Regex path (100 patterns) | Rift matches the path against 100 regexes. OpenAPI paths are templated, not regular expressions, and Microcks has no regex path dispatcher — a path template is a different matcher doing less work. |
| Complex AND/OR predicates | Rift evaluates `and`/`or` over method, path prefix and two alternative headers. Microcks has no header dispatcher for REST; the only expression is a Groovy `SCRIPT` dispatcher, which measures the scripting engine. |
| Header match (last of 100) | Same reason — no header dispatcher for REST. |
| JSON body equals | Rift matches method + path + an exact JSON body, but those 50 stubs sit on 50 *distinct* paths, so a Microcks translation would resolve on the path alone and never read the body — strictly less work. |
| JSONPath predicate | Microcks' `JSON_BODY` dispatcher uses a different expression dialect; equating them would be a claim about dialects, not matching cost. |
| XPath predicate | Microcks has no XPath dispatcher for REST. |
| Response templating | Microcks does have response templating, with a different expression language; the fixture's body marker is satisfied by a static example, so including it would report a static-response number under a templating label. |

### Reproduce it

```bash
gh workflow run benchmark-publish.yml \
  -f runner=ubuntu-16core -f reps=3 -f duration=20s -f connections=256 -f warmup=10s \
  -f microcks_only=true
```

The harness is in [`tests/benchmark`](https://github.com/achird-labs/rift/tree/master/tests/benchmark)
and runs both engines. If you think something here is unfair, re-run it — and if you find a
configuration that narrows the gap, [open an issue](https://github.com/achird-labs/rift/issues) and
we will publish the corrected number.

---

## A note on Microcks

Rift competes with Microcks on one axis — HTTP mock serving throughput — and Microcks wins on most
of the others. It is a well-engineered CNCF project solving a broader problem: keeping mocks and
tests honest against a specification, across protocols Rift does not speak. The gap measured on this
page is a consequence of a Spring Boot application with a datastore and a UI behind it, which is an
entirely sensible design for what it set out to do, rather than of anything done badly.

Microcks is a CNCF project. Rift is not affiliated with, endorsed by, or derived from it. All
comparative claims here are measured, and the methodology and harness are published so they can be
checked.
