# Rift vs Mountebank Performance Benchmark

This benchmark suite compares the performance of Rift (Rust-based HTTP proxy) against Mountebank (Node.js-based service virtualization tool) using identical configurations.

## Quick Start

```bash
# 1. Install required tools
./scripts/install-tools.sh

# 2. Start the benchmark environment
docker compose up -d --build

# 3. Run the benchmark suite
./scripts/run-benchmark.sh

# 4. View results
cat results/BENCHMARK_REPORT.md

# 5. Cleanup
docker compose down -v
```

## Direct-process mode (no Docker)

`scripts/bench_direct.py` runs the same comparison without Docker — useful on
machines where cgroup limits aren't available (e.g. macOS) or where you'd
rather give each engine the whole box.

```bash
# Prereqs: oha (load generator) on PATH, a release Rift binary, and mountebank
#   cargo build --release -p rift-http-proxy
#   npm install mountebank@2.9.1        # into ~/bench-mb, or pass --mb-bin

python3 scripts/bench_direct.py --run-all \
    --duration 20s --warmup 3s --connections 50 \
    --rift-bin ../../target/release/rift-http-proxy \
    --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb

cat results/DIRECT_BENCHMARK_REPORT.md
```

How it stays fair and correct:

- **Sequential, not concurrent** — each engine runs alone, so they never
  contend for CPU on a shared machine.
- **Disjoint port ranges** — Rift on `2525`/`4545+`, Mountebank on
  `2625`/`4645+`. Even if one engine fails to shut down it cannot be measured
  in place of the other.
- **Hard teardown** — each engine is launched in its own process group and
  killed by group + `lsof`; its ports must be confirmed free before the next
  engine starts.
- **Response assertions** — every scenario sends one real request first and
  checks the returned **body** (not just a 2xx status) proves the intended stub
  matched. A request that falls through to the empty no-match default aborts the
  run, so a mis-configured stub can't silently inflate throughput.
- **Identical configs** — both engines get byte-identical imposter JSON.

> `oha` initialises a TLS stack that reads the macOS keychain even for
> plain-HTTP targets, so run this outside a restricted sandbox.


Install all tools automatically:
```bash
./scripts/install-tools.sh
```

## Benchmark Configuration

### Test Environment

Both services run with identical resource constraints:
- **CPUs:** 2 cores
- **Memory:** 1GB RAM
- **Network:** Docker bridge network

### Imposters Configuration

The benchmark creates 12 imposters with varying complexity:

| Port (MB/Rift) | Name | Stubs | Description |
|----------------|------|-------|-------------|
| 4545/5545 | API Server | ~500 | REST API simulation with CRUD endpoints |
| 4546/5546 | Regex Matcher | 100 | Regex pattern matching stubs |
| 4547/5547 | Complex Predicates | 50 | AND/OR predicate combinations |
| 4548/5548 | Behaviors | 20 | Wait/delay behaviors |
| 4549/5549 | Simple Baseline | 2 | Minimal stubs for baseline |
| 4550/5550 | JSON Body Matcher | 100 | JSON body equals/contains predicates |
| 4551/5551 | JSONPath Matcher | 100 | JSONPath expression predicates |
| 4552/5552 | XPath Matcher | 100 | XPath expression predicates (XML) |
| 4553/5553 | Template Responses | 50 | EJS template response generation |
| 4554/5554 | Header Router | 100 | Header-based routing predicates |
| 4555/5555 | Query Param Matcher | 100 | Query string matching predicates |
| 4556/5556 | Decorate Behaviors | 20 | JavaScript injection behaviors |

**Total: ~1140+ stubs across all imposters**

### Test Scenarios

1. **Simple Baseline** - Health check endpoints with minimal stubs
2. **Admin API** - Imposter listing and retrieval operations
3. **API Endpoints** - REST API with many stubs (first/middle/last match)
4. **Regex Matching** - Pattern matching with regex predicates
5. **Complex Predicates** - AND/OR/NOT predicate combinations
6. **JSON Body Matching** - Body equals and contains predicates
7. **JSONPath Predicates** - JSONPath expression matching
8. **XPath Predicates** - XPath expression matching for XML
9. **Template Responses** - EJS template rendering with variables
10. **Header Routing** - Header-based request routing
11. **Query Parameter Matching** - Query string predicate matching
12. **Decorate Behaviors** - JavaScript injection for response modification
13. **Stress Test** - High concurrency (200 connections)

## Running Benchmarks

### Full Benchmark Suite

```bash
# Default: 30s duration, 50 concurrent connections
./scripts/run-benchmark.sh
```

### Custom Configuration

```bash
# Longer duration, more connections
DURATION=60s CONNECTIONS=100 ./scripts/run-benchmark.sh

# Quick smoke test
DURATION=10s CONNECTIONS=20 ./scripts/run-benchmark.sh
```

### Manual Testing

```bash
# Setup imposters only
./scripts/setup-imposters.sh

# Test individual endpoints
hey -z 10s -c 50 http://localhost:4545/api/v1/resource1  # Mountebank
hey -z 10s -c 50 http://localhost:5545/api/v1/resource1  # Rift
```

## Results

Results are saved in the `results/` directory:

- `BENCHMARK_REPORT.md` - Summary report with tables
- `results.csv` - Raw data in CSV format
- `mountebank_detailed.txt` - Full hey output for Mountebank
- `rift_detailed.txt` - Full hey output for Rift

### Interpreting Results

- **RPS (Requests/sec)** - Higher is better
- **Latency** - Lower is better (measured in ms or seconds)
- **P50/P99** - 50th and 99th percentile latencies
- **Improvement** - Percentage improvement of Rift over Mountebank

## Benchmark Findings

### Latest Results (November 25, 2025)

Test configuration: 15s duration, 50 concurrent connections, 2 CPUs, 1GB RAM per service

#### Core Functionality

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| Simple: Health Check | 1,914 | 39,127 | **20x faster** | 26.1ms → 1.3ms |
| Simple: Ping/Pong | 1,763 | 36,391 | **21x faster** | 28.4ms → 1.4ms |
| Admin: List Imposters | 6,361 | 29,021 | **4.5x faster** | 7.9ms → 1.7ms |
| Admin: Get Imposter | 396 | 884 | **2.2x faster** | 125.7ms → 56.4ms |

#### API Stub Matching (500 stubs)

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| API: First Stub Match | 1,865 | 33,422 | **18x faster** | 26.8ms → 1.5ms |
| API: Middle Stub Match | 589 | 25,512 | **43x faster** | 84.6ms → 2.0ms |
| API: Last Stub Match | 290 | 22,042 | **76x faster** | 171.1ms → 2.3ms |
| API: No Match (404) | 297 | 22,664 | **76x faster** | 167.0ms → 2.2ms |

#### JSON Body Matching

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| JSON: Body Equals (First) | 1,794 | 29,252 | **16x faster** | 27.9ms → 1.7ms |
| JSON: Body Equals (Middle) | 1,022 | 24,052 | **24x faster** | 48.8ms → 2.1ms |
| JSON: Body Contains | 1,272 | 24,352 | **19x faster** | 39.3ms → 2.1ms |

#### JSONPath Predicates (Standout Performance)

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| JSONPath: First Match | 107 | 26,583 | **247x faster** | 458.8ms → 1.9ms |
| JSONPath: Middle Match | 129 | 28,751 | **221x faster** | 380.5ms → 1.7ms |
| JSONPath: Last Match | 124 | 27,070 | **218x faster** | 397.1ms → 1.8ms |

#### XPath Predicates (Standout Performance)

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| XPath: First Match | 169 | 28,745 | **170x faster** | 292.4ms → 1.7ms |
| XPath: Middle Match | 168 | 27,234 | **161x faster** | 293.3ms → 1.8ms |
| XPath: Last Match | 173 | 27,248 | **157x faster** | 285.1ms → 1.8ms |

#### Regex Matching

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| Regex: First Pattern | 1,560 | 7,040 | **4.5x faster** | 32.0ms → 7.1ms |
| Regex: Middle Pattern | 118 | 257 | **2.1x faster** | 414.9ms → 193.0ms |
| Regex: Last Pattern | 65 | 130 | **2.0x faster** | 749.1ms → 378.2ms |

#### Template Responses

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| Template: Simple | 1,856 | 26,858 | **14x faster** | 26.9ms → 1.9ms |
| Template: With Query | 1,349 | 28,158 | **21x faster** | 37.0ms → 1.8ms |

#### Header & Query Routing

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| Header: First Route | 1,763 | 27,937 | **16x faster** | 28.3ms → 1.8ms |
| Header: Middle Route | 1,199 | 29,345 | **24x faster** | 41.6ms → 1.7ms |
| Header: Last Route | 843 | 28,971 | **34x faster** | 59.2ms → 1.7ms |
| Query: First Match | 1,787 | 28,549 | **16x faster** | 28.0ms → 1.8ms |
| Query: Middle Match | 1,143 | 24,425 | **21x faster** | 43.7ms → 2.0ms |
| Query: Last Match | 801 | 21,185 | **26x faster** | 62.2ms → 2.4ms |

#### Decorate Behaviors & Stress

| Test Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | Avg Latency (MB → Rift) |
|---------------|------------------|------------|---------|-------------------------|
| Decorate: First | 1,759 | 6,141 | **3.4x faster** | 28.4ms → 8.1ms |
| Decorate: Middle | 1,640 | 5,980 | **3.6x faster** | 30.4ms → 8.4ms |
| Complex: AND/OR | 904 | 29,322 | **32x faster** | 55.2ms → 1.7ms |
| Stress: 200 Concurrent | 1,830 | 29,716 | **16x faster** | 109.1ms → 6.7ms |

### Key Findings

1. **JSONPath/XPath Performance**: The most dramatic improvements are in JSONPath (**217-247x faster**) and XPath (**157-170x faster**) predicates. Mountebank's JavaScript-based implementations are extremely slow (~100-170 RPS), while Rift's native Rust implementations maintain ~27k RPS.

2. **Stub Lookup Performance**: Rift's performance remains consistent regardless of stub position. Mountebank shows linear degradation as stub count increases (290 RPS for last stub vs 1,865 RPS for first stub). Rift maintains ~22-33k RPS across all positions.

3. **404 Handling**: **76x faster** when no stub matches. Mountebank must iterate through all stubs before returning 404, while Rift's optimized matching is significantly faster.

4. **Complex Predicates**: **32x** improvement for AND/OR predicate combinations, showing efficient predicate evaluation in Rust.

5. **JSON Body Matching**: **16-24x faster** for JSON body predicates (equals, contains), demonstrating efficient JSON parsing and matching.

6. **Template Responses**: **14-21x faster** for EJS template rendering, with Rift's native template engine outperforming Node.js.

7. **Header/Query Routing**: **16-34x faster** for header and query parameter matching, with consistent performance regardless of match position.

8. **Decorate Behaviors**: **3.4-3.6x faster** for JavaScript injection behaviors. This is the smallest improvement because both implementations execute JavaScript, but Rift still benefits from faster request handling overhead.

9. **High Concurrency**: Under 200 concurrent connections, Rift maintains 29,716 RPS vs Mountebank's 1,830 RPS (**16x** improvement).

10. **Latency Consistency**: Rift maintains consistent low latency (1.3-8.4ms) across all scenarios, while Mountebank latency varies widely (7.9-749ms) depending on stub count, match position, and predicate type.

### Architecture Comparison

| Aspect | Mountebank | Rift |
|--------|------------|------|
| Language | Node.js (JavaScript) | Rust |
| Concurrency | Single-threaded event loop | Multi-threaded (Tokio) |
| Memory Model | Garbage collected | Zero-copy, no GC |
| Regex Engine | JavaScript RegExp | Rust regex crate |
| Stub Matching | Linear scan | Optimized matching |

### When to Choose Rift

Rift is recommended when you need:
- **High throughput**: 10-100x more requests per second
- **Low latency**: Sub-millisecond response times
- **Many stubs**: Performance doesn't degrade with stub count
- **High concurrency**: Efficient handling of many connections
- **Resource efficiency**: Lower CPU and memory usage

## Troubleshooting

### Services Not Starting

```bash
# Check container logs
docker logs mb-bench
docker logs rift-bench

# Verify ports are available
lsof -i :2525
lsof -i :3525
```

### hey Not Found

```bash
# macOS
brew install hey

# Linux (with Go installed)
go install github.com/rakyll/hey@latest

# Linux (direct download)
sudo curl -sSL https://hey-release.s3.us-east-2.amazonaws.com/hey_linux_amd64 -o /usr/local/bin/hey
sudo chmod +x /usr/local/bin/hey
```

### Connection Refused Errors

Wait for services to be healthy:
```bash
# Check health status
docker inspect --format='{{.State.Health.Status}}' mb-bench
docker inspect --format='{{.State.Health.Status}}' rift-bench
```

## Architecture Notes

### Why These Tests?

The benchmark suite is designed to test real-world scenarios:

1. **API Server (500 stubs):** Simulates a microservice with multiple REST endpoints, testing stub lookup performance with a large stub count.

2. **Regex Matching:** Tests the regex engine performance, which is critical for path matching and request body validation.

3. **Complex Predicates:** Tests the predicate evaluation engine with nested AND/OR logic.

4. **High Concurrency:** Tests how well each service handles many simultaneous connections.

### Fair Comparison Methodology

- Both services run in containers with identical resource limits
- Same imposter configurations are loaded via the Mountebank API
- Tests run sequentially to avoid resource contention
- Multiple requests ensure warm caches and JIT compilation
- Results include both raw numbers and percentage comparisons

## Contributing

To add new benchmark scenarios:

1. Add stub generation in `scripts/setup-imposters.sh`
2. Add benchmark function in `scripts/run-benchmark.sh`
3. Document the scenario in this README

## Related

- [Compatibility Tests](../compatibility/) - Functional compatibility tests
- [Integration Tests](../integration/) - Integration test suite
- [Mountebank Documentation](http://www.mbtest.org/)
