---
layout: default
title: Performance
nav_order: 8
permalink: /performance/
---

# Performance

Rift is designed for high performance, delivering **2-250x better throughput** than Mountebank.

---

## Benchmark Summary

Test configuration: 15s duration, 50 concurrent connections, 2 CPUs, 1GB RAM per service.

### Standout Results

| Feature | Mountebank | Rift | Speedup |
|:--------|:-----------|:-----|:--------|
| JSONPath Predicates | 107 RPS | 26,500 RPS | **247x faster** |
| XPath Predicates | 169 RPS | 28,700 RPS | **170x faster** |
| API Stub (Last Match) | 290 RPS | 22,000 RPS | **76x faster** |
| 404 Handling | 297 RPS | 22,600 RPS | **76x faster** |
| Complex Predicates | 904 RPS | 29,300 RPS | **32x faster** |
| JSON Body Matching | 1,022 RPS | 24,000 RPS | **24x faster** |
| Template Responses | 1,349 RPS | 28,100 RPS | **21x faster** |
| Simple Health Check | 1,914 RPS | 39,100 RPS | **20x faster** |
| High Concurrency | 1,830 RPS | 29,700 RPS | **16x faster** |

---

## Performance by Category

### Core Functionality

| Test | Mountebank (RPS) | Rift (RPS) | Speedup |
|:-----|:-----------------|:-----------|:--------|
| Health Check | 1,914 | 39,127 | **20x** |
| Ping/Pong | 1,763 | 36,391 | **21x** |
| List Imposters | 6,361 | 29,021 | **4.5x** |
| Get Imposter | 396 | 884 | **2.2x** |

### API Stub Matching (500 stubs)

| Test | Mountebank (RPS) | Rift (RPS) | Speedup |
|:-----|:-----------------|:-----------|:--------|
| First Stub Match | 1,865 | 33,422 | **18x** |
| Middle Stub Match | 589 | 25,512 | **43x** |
| Last Stub Match | 290 | 22,042 | **76x** |
| No Match (404) | 297 | 22,664 | **76x** |

### JSONPath Predicates

| Test | Mountebank (RPS) | Rift (RPS) | Speedup |
|:-----|:-----------------|:-----------|:--------|
| First Match | 107 | 26,583 | **247x** |
| Middle Match | 129 | 28,751 | **221x** |
| Last Match | 124 | 27,070 | **218x** |

### XPath Predicates

| Test | Mountebank (RPS) | Rift (RPS) | Speedup |
|:-----|:-----------------|:-----------|:--------|
| First Match | 169 | 28,745 | **170x** |
| Middle Match | 168 | 27,234 | **161x** |
| Last Match | 173 | 27,248 | **157x** |

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

### Latency Distribution

| Scenario | Mountebank P99 | Rift P99 |
|:---------|:---------------|:---------|
| Simple stub | 50ms | 2ms |
| Complex predicate | 150ms | 5ms |
| JSONPath match | 800ms | 3ms |
| Under load (200 conn) | 300ms | 15ms |

### Throughput Scaling

Rift maintains consistent throughput regardless of:
- Stub count (500+ stubs with minimal degradation)
- Stub position (first vs last stub match)
- Predicate complexity

Mountebank shows linear degradation as stub count increases.

---

## Running Benchmarks

### Prerequisites

```bash
# Install hey (HTTP load generator)
brew install hey  # macOS
# or
go install github.com/rakyll/hey@latest
```

### Run Benchmark Suite

```bash
cd tests/benchmark

# Start services
docker compose up -d --build

# Run benchmarks
./scripts/run-benchmark.sh

# View results
cat results/BENCHMARK_REPORT.md

# Cleanup
docker compose down -v
```

### Custom Configuration

```bash
# Longer duration, more connections
DURATION=60s CONNECTIONS=100 ./scripts/run-benchmark.sh

# Quick smoke test
DURATION=10s CONNECTIONS=20 ./scripts/run-benchmark.sh
```

---

## Optimization Tips

### For Maximum Throughput

1. **Use specific predicates** - `equals` is faster than `matches`
2. **Order stubs by frequency** - Most-matched stubs first
3. **Avoid unnecessary behaviors** - Each behavior adds overhead
4. **Use native formats** - JSON body predicates are faster than string matching

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
| **Rift** | Rust | 20,000-40,000 | High-performance mocking |
| Mountebank | Node.js | 500-2,000 | Feature-rich service virtualization |
| WireMock | Java | 1,000-5,000 | Java ecosystem integration |
| MockServer | Java | 1,000-3,000 | Contract testing |

Rift provides 10-100x better performance while maintaining Mountebank compatibility.

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
cargo build --release --no-default-features --features redis-backend,lua,javascript
```

Only the `rift-http-proxy` binary is affected; `rift-core` and the FFI crate use the system
allocator.

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
**deliberately not used**: Rift runs each script (Boa / mlua) on a `spawn_blocking` worker so a
buggy or non-yielding script is isolated, and a panic there is contained by the async runtime as a
`JoinError` rather than crashing the server — which relies on unwinding. Under `panic = "abort"` a
single bad script would abort the whole process. Adopting it would require re-validating the
scripting and fault paths first, so it stays off pending that work.
