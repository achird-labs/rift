---
layout: default
title: Performance
nav_order: 8
permalink: /performance/
---

# Performance

Rift delivers **20–6,000x** the throughput of Mountebank on identical imposter
configs, with sub-millisecond tail latency that stays flat as stub count grows.

---

## Benchmark Summary

The suite was run on two deliberately different hosts. Publishing both is the point:
the multiplier depends heavily on the machine, and a single number would overstate
the result.

- **Apple M4 laptop** (10 cores, macOS) — the conservative read
- **AMD EPYC 9V74** (16 vCPU, 62 GiB, Linux) — the server read

Rift built from `master` (`924cf73`) · Mountebank 2.9.1 · `oha`, 50 keep-alive
connections, 20s/scenario after warmup, native processes (no Docker), each engine run
alone. Every figure is the **median of 3 repetitions**. Measured 2026-07-20. Full
method and reproduction:
[`tests/benchmark`](https://github.com/achird-labs/rift/tree/master/tests/benchmark).

| Scenario | MB (M4) | Rift (M4) | M4 speedup | MB (EPYC) | Rift (EPYC) | EPYC speedup |
|:---------|--------:|----------:|:-----------|----------:|------------:|:-------------|
| Regex (100th pattern) | 112 | 207,024 | **1,857x** | 52 | 317,851 | **6,160x** |
| API stub — no match (404) | 1,351 | 209,763 | **155x** | 549 | 332,574 | **606x** |
| API stub — last match | 1,344 | 209,523 | **156x** | 542 | 322,530 | **595x** |
| API stub — middle match | 3,437 | 210,151 | **61x** | 1,081 | 324,067 | **300x** |
| API stub — first match | 8,546 | 211,378 | **25x** | 5,728 | 323,408 | **57x** |
| Query-param routing | 2,751 | 164,133 | **60x** | 1,112 | 211,748 | **190x** |
| Header routing | 3,016 | 158,596 | **53x** | 1,202 | 201,940 | **168x** |
| Complex AND/OR predicates | 4,703 | 191,987 | **41x** | 1,814 | 259,548 | **143x** |
| JSONPath predicates | 4,312 | 199,404 | **46x** | 1,921 | 304,796 | **159x** |
| XPath predicates | 5,542 | 187,869 | **34x** | 1,966 | 247,897 | **126x** |
| JSON body matching | 7,611 | 199,670 | **26x** | 2,730 | 294,294 | **108x** |
| Template responses | 9,022 | 194,236 | **22x** | 3,152 | 283,815 | **90x** |
| Simple static stub | 8,898 | 214,818 | **24x** | 5,982 | 324,952 | **54x** |

### How to read the two columns

Going from the laptop to the 16-vCPU server, **Rift gets faster (215k → 325k) and
Mountebank gets slower (8,898 → 5,982)**. Mountebank is single-threaded, so it can
only use one core, and this server's individual cores are slower than the M4's — it
gains nothing from the other 15. Rift uses them all.

That means the EPYC multipliers are inflated at *both* ends, and the honest headline
is the M4 column. It is still 22x–1,857x.

Tail latency is the more stable comparison: Rift's p99 is **0.43–0.97 ms on both
hosts**, while Mountebank's ranges from 2.9 ms to 1.7 *seconds* depending on scenario.

> Measurement caveat: a laptop thermally throttles under a 30-minute run — both
> engines lost ~7% between the first and last repetition, and per-scenario spread
> reached 12% on the M4 versus 5% on EPYC. Treat M4 figures as ±10%.

---

## Why throughput stays flat

Rift holds ~210k RPS (M4) / ~325k RPS (EPYC) whether the matching stub is first,
middle, or last — and on a no-match 404 — while Mountebank degrades linearly with
stub count:

| API stub position | Mountebank (RPS) | Rift (RPS) | Speedup |
|:------------------|:-----------------|:-----------|:--------|
| First | 8,546 | 211,378 | **25x** |
| Middle | 3,437 | 210,151 | **61x** |
| Last | 1,344 | 209,523 | **156x** |
| No match (404) | 1,351 | 209,763 | **155x** |

Regex used to be the exception on Rift's side too — it can't be hash-dispatched, and
at the 100th pattern Rift managed ~54k RPS against Mountebank's 106. The
candidate-bitset matching framework removed that cliff: regex now runs at **207k RPS**,
in line with every other predicate type. Mountebank's per-stub JS `RegExp` scan still
collapses to 112 RPS at the 100th pattern, so the gap widened from 515x to **1,857x**
— not because Mountebank got slower, but because Rift stopped having a slow path.

On the admin control plane, creating 1,000 fully-overlapping stubs (the O(n²) case
issue #423 fixed) takes Rift 6.6ms vs Mountebank's 114.7ms, and grows memory +9MB vs
+51MB — while Rift additionally computes stub-overlap warnings Mountebank does not.

---

## Why Is Rift Faster?

### Architecture Comparison

| Aspect | Mountebank | Rift |
|:-------|:-----------|:-----|
| **Language** | Node.js (JavaScript) | Rust |
| **Concurrency** | Single-threaded event loop | Multi-threaded (Tokio) |
| **Memory Model** | Garbage collected | Zero-copy, no GC |
| **Regex Engine** | JavaScript RegExp | Rust regex crate |
| **JSON Parsing** | JavaScript JSON | serde_json (SIMD) |
| **Stub Matching** | Linear scan | Optimized matching |

### Key Optimizations

1. **Native Code**: Rust compiles to native machine code, avoiding interpreter overhead.

2. **Async I/O**: Tokio runtime provides efficient async networking with work-stealing scheduler.

3. **Zero-Copy Parsing**: serde_json parses JSON without unnecessary allocations.

4. **Efficient Regex**: Rust's regex crate uses finite automata for O(n) matching.

5. **Connection Pooling**: Reuses connections to upstream services.

6. **Thread Pool**: Dedicated workers for script execution.

---

## Performance Characteristics

### Latency (p99)

| Scenario | Mountebank | Rift |
|:---------|:-----------|:-----|
| Exact stub match (last of 500) | 40ms | 0.6ms |
| Complex AND/OR predicate | 17ms | 0.8ms |
| JSONPath match | 17ms | 1.0ms |
| Regex (100th pattern) | 641ms | 1.8ms |

### Throughput Scaling

Rift maintains consistent throughput regardless of:
- Stub count (500+ stubs with minimal degradation)
- Stub position (first vs last stub match)
- Predicate complexity

Mountebank shows linear degradation as stub count increases.

---

## Running Benchmarks

The suite runs both engines as native processes, one at a time on disjoint ports,
and posts byte-identical imposter JSON to each. See
[`tests/benchmark/README.md`](https://github.com/achird-labs/rift/tree/master/tests/benchmark)
for full details.

### Prerequisites

```bash
cargo build --release -p rift-http-proxy          # build Rift from source
cargo install oha                                 # load generator
npm install --prefix ~/bench-mb mountebank@2.9.1  # reference engine
```

### Run the suite

```bash
cd tests/benchmark

# Serving throughput + tail latency
python3 scripts/bench_direct.py --run-all \
    --duration 20s --warmup 3s --connections 50 \
    --rift-bin ../../target/release/rift-http-proxy \
    --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb
cat results/DIRECT_BENCHMARK_REPORT.md

# Admin create/read (imposter creation + overlap analysis)
python3 scripts/bench_admin.py --run-all \
    --rift-bin ../../target/release/rift-http-proxy \
    --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb
cat results/ADMIN_BENCHMARK_REPORT.md
```

> `oha` reads the macOS keychain to initialise TLS even for plain-HTTP targets —
> run outside a restricted sandbox.

---

## Optimization Tips

### For Maximum Throughput

1. **Use specific predicates** - `equals` is faster than `matches`
2. **Order stubs by frequency** - Most-matched stubs first
3. **Avoid unnecessary behaviors** - Each behavior adds overhead
4. **Use native formats** - JSON body predicates are faster than string matching

### For Script Fault Injection

Script fault decisions are memoized in a decision cache, keyed on the request. By default the key
includes **every** request header. That is always correct, but if your traffic carries a
per-request-unique header — `x-request-id`, `traceparent`, `x-amzn-trace-id`, `date` — then every
key is unique, nothing ever hits, and the cache becomes pure overhead: it pays hashing, allocation
and lock traffic on the hot path and returns nothing.

Rift cannot narrow the key for you: the cached value is *your* script's decision, and your script is
handed every header, so it may branch on any of them. Dropping a header from the key that your
script actually reads would serve one request's decision to a different request. So the allowlist is
opt-in — it is your assertion about what your scripts read:

```yaml
# Proxy config (the same file that carries `script_rules`) — NOT the imposter `_rift` block.
listen:
  port: 8080
script_rules:
  - # ...
decision_cache:
  enabled: true
  max_size: 10000
  ttl_seconds: 300
  key_headers: ["X-Tenant", "X-Feature-Flag"]
```

Only the listed headers enter the cache key; names are matched case-insensitively, and an empty
list (`[]`) declares that no header affects your decisions. Your scripts still receive **all**
headers either way — this only changes what makes two requests "the same" for caching.

If the cache degenerates to a ~0% hit rate, Rift logs a warning once per process telling you so,
rather than silently burning CPU.

#### What makes two requests "the same"

The key is the method, the path, the **query string**, the `key_headers` above, the rule id, and the
**body**.

The query is keyed on its **raw spelling**, so `?a=1&b=2` and `?b=2&a=1` are two entries even though
they mean the same thing. That is deliberate: it can only cost you a cache miss, whereas keying on
the parsed form could hand one request another's decision. Clients serialize query strings
deterministically, so in practice it costs nothing.

How the body counts depends on whether it is JSON:

- **JSON** — keyed *structurally*, so whitespace and key order do not split the key. Two requests
  whose bodies parse to the same value share one entry. The corollary: a script that branches on
  the raw *formatting* of a valid-JSON body is outside the cache-key contract, the same way one
  that reads a header you left out of `key_headers` is.
- **Anything else** — binary, plain text, malformed JSON, or an empty body — is keyed on its raw
  bytes, which is what your script reads via `ctx.request.raw_body`. Two different uploads are two
  different keys.

The two are kept in separate hash domains, so a JSON `null` body, an empty body, and a binary body
are always three distinct keys.

> The cache is only consulted on the fault-injection proxy path with `script_rules` configured and
> flow state **not** configured — stateful scripts are never cached.

### For Lowest Latency

1. **Minimize stub count** - Fewer stubs = faster matching
2. **Use simple responses** - Static `is` responses are fastest
3. **Avoid injection** - JavaScript execution adds latency
4. **Enable connection pooling** - Reuse upstream connections

### Resource Allocation

```yaml
# Recommended for high throughput
resources:
  requests:
    cpu: 1000m
    memory: 256Mi
  limits:
    cpu: 2000m
    memory: 512Mi
```

---

## Rift vs WireMock

WireMock is the most widely used JVM mock server, so this is the comparison most teams migrating to
Rift actually care about. It runs as its own suite (`tests/benchmark/scripts/bench_wiremock.py`)
because WireMock cannot consume imposter JSON — its mappings are *generated* from the same fixture
`bench_direct.py` uses, so the two suites cannot drift onto different workloads.

**Rift is 4.0x–14.0x WireMock's throughput**, and holds a p99 of ~2.5 ms where WireMock's climbs
from 7 ms to 31 ms as matching work grows.

| Scenario | WireMock (256t) | Rift | Rift/WM | WireMock p99 | Rift p99 |
|:---------|----------------:|-----:|--------:|-------------:|---------:|
| Simple static stub | 83,048 | 334,025 | **4.0x** | 7.0 ms | 2.4 ms |
| API first stub | 78,906 | 326,778 | **4.1x** | 7.5 ms | 2.5 ms |
| API middle stub | 42,668 | 326,601 | **7.7x** | 15.8 ms | 2.5 ms |
| Deep path match (410 stubs) | 24,264 | 326,779 | **13.5x** | 31.6 ms | 2.5 ms |
| No match | 24,589 | 345,114 | **14.0x** | — | — |
| Regex path (100 patterns) | 48,982 | 311,815 | **6.4x** | — | — |
| Complex AND/OR predicates | 56,541 | 251,170 | **4.4x** | — | — |
| JSON body equals | 63,814 | 314,939 | **4.9x** | — | — |
| JSONPath predicate | 63,995 | 310,350 | **4.8x** | — | — |
| XPath predicate | 46,408 | 231,868 | **5.0x** | — | — |
| Response templating | 48,541 | 260,762 | **5.4x** | — | — |
| Header match (last of 100) | 24,539 | 180,255 | **7.3x** | — | — |
| Query match (last of 100) | 20,529 | 190,867 | **9.3x** | — | — |

<sub>Intel Xeon Platinum 8573C, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-27. WireMock 3.9.1 on
Temurin 21, Rift built from `master`. `oha` at 256 keep-alive connections, 20s per scenario after a
10s warmup — identical settings for both engines, which each ran alone. Median of 3 repetitions;
spread ≤5.6% for WireMock, ≤1.2% for Rift. Reproduce with
`gh workflow run benchmark-publish.yml -f connections=256 -f run_sweep=false`.</sub>

### The gap widens with matching work

Rift is roughly flat across the suite — 334k on a trivial stub, 327k on a 410-stub deep path match.
WireMock falls from 83k to 24k on the same pair. That shape, not the headline multiple, is the
thing worth understanding: Rift's matcher cost is small relative to its per-request overhead, so
adding stubs barely moves it, while WireMock pays per candidate.

### Why WireMock's thread pool is pinned, and why it is not a thumb on the scale

WireMock is thread-per-request. Its default pool is 10 threads, which bounds in-flight requests at
10 — far below the 256 connections offered here. Benchmarking against that default would measure
the pool, not the engine, and any WireMock user would rightly reject the result. So the headline
series pins the Jetty pool to `max(cores, connections)`.

**The pin is a fairness guarantee, not a speedup.** The stock 10-thread column is published beside
it, and on this hardware the two are within noise of each other — 86,840 vs 83,048 on a simple
stub. The CPU saturates before the thread pool binds, so the pin changed essentially nothing. The
gap is not an artefact of how WireMock was configured.

WireMock's request journal is also **off** (`--no-request-journal`). It records every request
unbounded by default, and Rift and Mountebank are both measured with recording off; leaving it on
would compare WireMock-with-recording against Rift-without. In our own measurements that one flag
was worth ~30% throughput and ~5x p99.9 to WireMock.

### Two caveats we will not bury

**`complex_predicate` is not a pure predicate comparison.** WireMock cannot express an OR across
two *different* headers within one stub, so that imposter's 50 stubs become 101 mappings and the
measured request matches the 50th candidate where Rift matches the 25th — roughly twice the scan.
That is a genuine cost of modelling the workload in WireMock, but it is not like-for-like, and the
4.4x on that row should be read with it in mind.

**The all-2xx sanity gate is weaker for WireMock.** WireMock 404s an unmatched request, so the
suite installs a catch-all empty-200 to reproduce Rift's no-match default — which means an all-2xx
status distribution no longer proves anything matched. The per-scenario body-marker assertion is
the gate that actually catches a mistranslated stub.

### Throughput depends on offered concurrency — quote the connection count

At 50 connections Rift's advantage is smaller. This is not noise, and it is not Rift slowing down:
at 50 connections Rift is bounded by the **closed-loop harness** rather than by its own capacity.
Its per-request service time is small enough that `connections ÷ latency` caps throughput before
the engine does — observed throughput tracks that quotient within ~10% at every connection count we
have measured. WireMock is engine-bound at both points, so the ratio compresses. **Any
Rift-vs-WireMock number is meaningless without its connection count attached.**

| Scenario | Mountebank | WireMock (stock, 10t) | WireMock (50t) | Rift | Rift/WM |
|:---------|-----------:|----------------------:|---------------:|-----:|--------:|
| Simple static stub | 4,247 | 64,611 | 65,602 | 204,687 | **3.1x** |
| API first stub | 4,149 | 61,407 | 61,259 | 190,659 | **3.1x** |
| API middle stub | 817 | 37,262 | 37,066 | 184,714 | **5.0x** |
| Deep path match (410 stubs) | 417 | 22,867 | 22,846 | 183,847 | **8.0x** |
| No match | 420 | 23,383 | 23,310 | 188,958 | **8.1x** |
| Regex path (100 patterns) | 38 | 40,512 | 41,274 | 179,502 | **4.3x** |
| Complex AND/OR predicates | 1,322 | 45,300 | 44,957 | 152,548 | **3.4x** |
| JSON body equals | 2,001 | 50,722 | 50,637 | 176,988 | **3.5x** |
| JSONPath predicate | 1,436 | 50,304 | 50,930 | 175,149 | **3.4x** |
| XPath predicate | 1,477 | 39,812 | 39,493 | 144,593 | **3.7x** |
| Response templating | 2,339 | 39,769 | 39,572 | 162,102 | **4.1x** |
| Header match (last of 100) | 892 | 22,961 | 22,699 | 125,173 | **5.5x** |
| Query match (last of 100) | 833 | 19,770 | 19,617 | 130,354 | **6.6x** |

<sub>Same host and method as the 256-connection table (Intel Xeon Platinum 8573C, 16 vCPU,
`ubuntu-16core`), 2026-07-27, 50 keep-alive connections, 20s per scenario after a 10s warmup,
median of 3 repetitions. Spread ≤4.8% for every engine. Rift's p99 is 0.83–0.97 ms here against
WireMock's 2.9–6.8 ms.</sub>

Rift is **3.1x–8.1x** WireMock at 50 connections, against 4.0x–14.0x at 256. Both engines post
lower absolute throughput at 50 connections than at 256 — Rift 204,687 vs 334,025 on a simple stub,
WireMock 65,602 vs 83,048 — so this is not WireMock catching up. It is Rift having less room to use
the machine.

The pin is a no-op at this connection count too: stock 10-thread WireMock (64,611) and the
50-thread series (65,602) are within a percent of each other, so nothing here depends on how
WireMock's pool was configured.
## Comparison with Alternatives

Measured, not estimated. Every figure below comes from the same run of the same 13-scenario suite
on the same host — see [Rift vs WireMock](#rift-vs-wiremock) for the per-scenario breakdown and the
caveats that go with it.

| Tool | Language | Measured RPS<br><sub>simple stub → deep path match</sub> | Best For |
|:-----|:---------|:---------|:---------|
| **Rift** | Rust | **334,025 → 326,779** | Large stub sets, polyglot orgs, in-process embedding |
| WireMock | Java | 83,048 → 24,264 | The mature default: deep JVM/Spring integration, extensions, OpenAPI-driven mocking, commercial support |
| Mountebank | Node.js | 4,309 → 419 | Protocols beyond HTTP (TCP, SMTP), and the API/config format Rift implements |

<sub>Intel Xeon Platinum 8573C, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-27. `oha` at 256
keep-alive connections, 20s per scenario after a 10s warmup, native processes, each engine run
alone. Median of 3 repetitions; per-scenario spread ≤5.6% for every engine (Rift ≤1.2%).
WireMock 3.9.1 with its Jetty pool pinned to 256 and its request journal off; Mountebank 2.9.1.</sub>

These are architecture comparisons, not quality judgements. Both engines are well-built and more
widely adopted than Rift, and each does things Rift does not — see
[Rift vs WireMock]({{ site.baseurl }}/comparisons/wiremock/) for both directions.

**Microcks** is compared separately, in [Rift vs Microcks]({{ site.baseurl }}/comparisons/microcks/),
and is deliberately *not* a row in the table above: its figures come from a different dispatch, and
the table's value is that every cell in it came from one run on one host. That page also carries the
more interesting finding — Microcks does **not** pay per candidate stub the way Mountebank and
WireMock do (stub *position* costs it nothing), so "throughput stays flat" distinguishes Rift from
Microcks on absolute throughput and tail latency rather than on scan behaviour.

<sub>WireMock and WireMock Cloud are products of WireMock Inc.; Mountebank is an independent open
source project. Rift is not affiliated with, endorsed by, or derived from either. All comparative
claims here are measured, and the harness is published so they can be checked.</sub>

---

## Runtime Socket Tuning

Rift tunes accepted sockets for low latency out of the box and exposes a couple of knobs via
environment variables:

| Variable | Default | Effect |
|:---------|:--------|:-------|
| `RIFT_TCP_NODELAY` | on | `TCP_NODELAY` is set on every accepted socket (disables Nagle's algorithm) for lower request latency. Set `false`/`0`/`off` to disable. |
| `RIFT_TCP_BACKLOG` | `1024` | Listen backlog (queue depth) for the accept loop. A larger backlog absorbs bigger connection bursts. Non-positive or unparsable values fall back to the default. |

These apply to both the imposter and proxy accept loops.

## Memory Allocator (mimalloc)

The `rift-http-proxy` binary uses the [mimalloc](https://github.com/microsoft/mimalloc) global
allocator by default — it improves throughput under the allocation-heavy request path. It is a
Cargo feature named `mimalloc`, enabled in the binary's default feature set:

```bash
# Default build — mimalloc is on
cargo build --release

# Drop it (e.g. for a cross-compile or FFI build) by opting out of default features
cargo build --release --no-default-features --features redis-backend,javascript

# Or swap in jemalloc (bake-off candidate, issue #717)
cargo build --release --no-default-features --features redis-backend,javascript,jemalloc
```

An opt-in `jemalloc` feature builds the binary with
[tikv-jemallocator](https://github.com/tikv/jemallocator) instead, for A/B allocator
comparison; if both allocator features are enabled (e.g. `--all-features`), mimalloc takes
precedence. The startup log reports which allocator is active (`Global allocator: …`), and the
benchmark harness automates the three-way comparison — see the allocator bake-off section in
`tests/benchmark/README.md`.

Only the `rift-http-proxy` binary is affected; `rift-mock-core` and the FFI crate use the system
allocator.

## Runtime Topology (per-core, experimental)

By default the `rift-http-proxy` binary runs one multi-threaded, work-stealing Tokio runtime that
serves everything — imposter accept loops, per-connection work, the admin API, and metrics. That
is the right default and is unchanged. For **Linux hosts under high connection counts**, an opt-in
alternative topology (RFC-712) trades a little complexity for **materially lower tail latency**:

```bash
# Default — one work-stealing runtime (unchanged behaviour)
rift-http-proxy --runtime work-stealing

# Per-core: N single-threaded runtimes, N = physical cores
rift-http-proxy --runtime per-core

# …or pin the worker count explicitly
rift-http-proxy --runtime per-core=8

# Env-var equivalent (the CLI flag wins if both are set)
RIFT_RUNTIME=per-core rift-http-proxy
```

In per-core mode each imposter port binds **one `SO_REUSEPORT` listener per worker runtime**, and
each accept loop runs on its own single-threaded runtime. The kernel spreads incoming connections
across the listeners by 4-tuple hash, so a connection lives and dies on one core — no cross-core
wake-ups and no work-stealing overhead. The control plane (admin API, metrics, imposter
create/delete) stays on a small shared runtime; only the request-serving accept loops fan out.

At startup the binary reports the topology it actually resolved to, next to the allocator line:

```
INFO rift: Runtime topology: per-core x8
```

### What it actually buys you

Measured on Linux x86-64 (AMD EPYC, engine pinned to 2/4/8 vCPU with the load generator on disjoint
physical cores, 3 repetitions, 14 scenarios — issue
[#746](https://github.com/achird-labs/rift/issues/746)):

| | per-core vs work-stealing |
|:---|:---|
| **p99 latency** | **18–35% lower** at every core count tested, at both 256 and 512 connections |
| **p999 latency** | lower in **every** scenario measured (84/84 points) |
| **Oversubscription** | at 2 vCPU / 512 connections work-stealing hit a ~20 ms p99 cliff; per-core stayed at ~5.6 ms |
| **Throughput** | **+1–4%** — at or below run-to-run noise; treat it as unchanged |
| **Scaling with cores** | **no measured difference**: both topologies scaled ~4.2× for 4× the cores |

The headline is tail latency, not throughput. If your mock server's p99 shows up in someone's CI
timing budget, per-core is worth benchmarking; if you are chasing raw RPS, it will not move.

### When to use it

- **Use per-core** on a **Linux** host that serves high connection counts and where **tail latency
  matters** — and measure it for *your* workload before committing (see
  [Running Benchmarks](#running-benchmarks); the harness's `--runtime` flag benches both).
- **Keep the default** on small hosts, low-concurrency workloads, or any non-Linux platform.
- **Do not** switch expecting more throughput, or better scaling as you add cores. Neither was
  observed.

> **Experimental.** Per-core mode is opt-in and off by default. Its functional behaviour is
> validated on Linux and the latency benefit above is measured, but on a single machine class at up
> to 4 physical cores. Behaviour on much larger hosts is not yet characterised
> ([#774](https://github.com/achird-labs/rift/issues/774)) — benchmark it for your workload rather
> than enabling it blanket.

### Platform matrix

| Platform | Per-core mode | Behaviour |
|:---------|:--------------|:----------|
| **Linux** (x86-64 / aarch64) | First-class | `SO_REUSEPORT` balances accepts across the listener group by 4-tuple hash — the design's premise. |
| **macOS** | Falls back, with a warning | BSD/XNU `SO_REUSEPORT` does **not** hash-balance TCP accepts across the group (they skew to one socket), so per-core would funnel most connections to one worker — worse than work-stealing. The binary logs the fallback and runs work-stealing; dev boxes lose nothing. |
| **Windows** | Not offered | No `SO_REUSEPORT` semantics; the flag is rejected at startup. |

Because macOS silently falls back, always confirm the effective topology from the startup
`Runtime topology:` line rather than assuming the requested mode took effect.

### CPU affinity

`--runtime-affinity` (or `RIFT_RUNTIME_AFFINITY=1`) pins each per-core worker thread to a CPU core.
It is **off by default** and only meaningful with `--runtime per-core`; the effect is real on Linux
and advisory elsewhere. Leave it off when other processes contend for the same cores — pinning under
contention hurts tail latency more than the cache-locality gain is worth.

### Blocking pool

Each per-core runtime owns its own `spawn_blocking` pool (used by JavaScript inject scripts and
blocking flow-store backends). To keep the *total* thread count near a single runtime's, each
worker's pool is clamped rather than defaulting to 512 threads apiece — so N workers do not
multiply into N×512 blocking threads. Note that a few synchronous script paths — notably a
JavaScript `wait` function that computes a delay — run inline on the calling worker rather than on
the blocking pool, so keep such scripts cheap under per-core.

### Observing load spread

`SO_REUSEPORT` balances by connection 4-tuple, so a load generator using **few source addresses**
(or few connections) can leave workers unevenly loaded. Benchmark with many connections (≥256) and
watch the per-worker accept counter to see the real spread:

```bash
curl -s localhost:9090/metrics | grep rift_accepted_connections_total
# rift_accepted_connections_total{worker="0"} 63
# rift_accepted_connections_total{worker="1"} 54
# rift_accepted_connections_total{worker="2"} 75
# rift_accepted_connections_total{worker="3"} 64
```

The `worker` label is the accept-loop slot — the worker index under per-core, or a single `0` in
the default topology. See [Metrics]({{ site.baseurl }}/features/metrics/) for the full metric set,
and the [CLI Reference]({{ site.baseurl }}/configuration/cli/) for `--runtime` / `--runtime-affinity`
and their env-var aliases.

## Build Tuning

The shipped release profile is already aggressive:

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
strip = true
```

For the last few percent on **self-hosted** deployments you can tune the build further. These are
opt-in because they trade portability or compile time for throughput.

### `target-cpu=native` (recommended for self-hosted)

Build for the exact CPU you run on so the compiler can use the newest SIMD/AVX instructions:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

or persist it in `.cargo/config.toml`:

```toml
[build]
rustflags = ["-C", "target-cpu=native"]
```

**Caveat:** the resulting binary is **not portable** — it may crash with `SIGILL` on an older or
different CPU. Use it only when you build on (or for) the same microarchitecture you deploy to; the
published release artifacts deliberately omit it so they run everywhere.

### `lto = "fat"`

Fat LTO optimizes across the whole dependency graph rather than per-crate (thin). Expect **small,
single-digit-percent** gains at the cost of a **substantially longer release build**. It is *not*
enabled by default: the compile-time cost is not worth it for CI/release, and the win should be
confirmed against the performance regression gate (see the CI perf gate) before adopting. To try it
locally, set `lto = "fat"` under `[profile.release]`.

### `panic = "abort"` — not adopted

`panic = "abort"` removes unwinding machinery (smaller binary, marginally faster). It is
**deliberately not used**: Rift runs each script (Boa) on a `spawn_blocking` worker so a
buggy or non-yielding script is isolated, and a panic there is contained by the async runtime as a
`JoinError` rather than crashing the server — which relies on unwinding. Under `panic = "abort"` a
single bad script would abort the whole process. Adopting it would require re-validating the
scripting and fault paths first, so it stays off pending that work.
