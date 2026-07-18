---
layout: default
title: Performance
nav_order: 8
permalink: /performance/
---

# Performance

Rift delivers **10–500x** the throughput of Mountebank on identical imposter
configs, with sub-millisecond tail latency that stays flat as stub count grows.

---

## Benchmark Summary

Native processes on Apple M4 (10 cores) / macOS · Rift 0.1.0 · Mountebank 2.9.1 ·
`oha`, 50 connections, 20s/scenario. Full method and reproduction:
[`tests/benchmark`](https://github.com/EtaCassiopeia/rift/tree/master/tests/benchmark).

| Feature | Mountebank | Rift | Speedup |
|:--------|:-----------|:-----|:--------|
| Regex (100th pattern) | 106 RPS | 54,434 RPS | **515x** |
| API stub — no match (404) | 1,309 RPS | 206,865 RPS | **158x** |
| API stub — last match | 1,351 RPS | 201,403 RPS | **149x** |
| Query-param routing | 2,366 RPS | 118,228 RPS | **50x** |
| Header routing | 3,050 RPS | 148,739 RPS | **49x** |
| Complex AND/OR predicates | 4,646 RPS | 181,320 RPS | **39x** |
| JSONPath predicates | 4,480 RPS | 173,671 RPS | **39x** |
| XPath predicates | 5,552 RPS | 174,567 RPS | **31x** |
| JSON body matching | 7,802 RPS | 188,247 RPS | **24x** |
| Template responses | 9,446 RPS | 189,649 RPS | **20x** |

> Absolute numbers are unconstrained (whole machine) and scale with hardware. What's
> stable across machines is the *shape*: Rift stays flat where Mountebank degrades.

---

## Why throughput stays flat

Rift holds ~120k–210k RPS whether the matching stub is first, middle, or last — and
on a no-match 404 — while Mountebank degrades linearly with stub count:

| API stub position | Mountebank (RPS) | Rift (RPS) | Speedup |
|:------------------|:-----------------|:-----------|:--------|
| First | 8,124 | 209,555 | **26x** |
| Middle | 3,071 | 198,504 | **65x** |
| Last | 1,351 | 201,403 | **149x** |
| No match (404) | 1,309 | 206,865 | **158x** |

Regex is the exception on *both* sides: it can't be hash-dispatched, so it's Rift's
own slowest matcher (~54k RPS) — but Mountebank's per-stub JS `RegExp` scan collapses
to 106 RPS at the 100th pattern, a 515x gap.

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
[`tests/benchmark/README.md`](https://github.com/EtaCassiopeia/rift/tree/master/tests/benchmark)
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

## Comparison with Alternatives

| Tool | Language | Typical RPS | Best For |
|:-----|:---------|:------------|:---------|
| **Rift** | Rust | 100,000+ | High-performance mocking |
| Mountebank | Node.js | 500-2,000 | Feature-rich service virtualization |
| WireMock | Java | 1,000-5,000 | Java ecosystem integration |
| MockServer | Java | 1,000-3,000 | Contract testing |

Rift provides 10-500x better performance while maintaining Mountebank compatibility.
(Rift's figure is native/unconstrained; it scales with hardware.)

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
is the right default and is unchanged. For **many-core Linux hosts under high connection counts**,
an opt-in alternative topology (RFC-712) can change the shape of throughput-vs-cores:

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

### When to use it

- **Use per-core** on an 8-core-or-larger **Linux** server that must sustain high concurrency, and
  measure it against work-stealing for *your* workload before committing (see
  [Running Benchmarks](#running-benchmarks); the harness's `--runtime` flag benches both).
- **Keep the default** on small hosts, latency-light workloads, or any non-Linux platform.

> **Experimental.** Per-core mode is opt-in and off by default. Its functional behaviour is
> validated on Linux, but the throughput decision on ≥8-core hardware is still being finalised
> (tracking issue [#746](https://github.com/EtaCassiopeia/rift/issues/746)); treat it as a knob to
> benchmark, not a blanket recommendation, until that lands.

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
