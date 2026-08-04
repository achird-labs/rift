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

Three comparisons live on this page, each with its own hardware, date and engine versions, because
mixing them would produce a number nothing measured:

- **[vs Mountebank](#benchmark-summary)** (below) — two hosts, 50 connections, 2026-07-20.
- **[vs WireMock](#rift-vs-wiremock)** — 256 and 50 connections, 2026-07-27.
- **[vs Microcks](#rift-vs-microcks)** — 256 connections, 2026-07-30.

And if you only read one section, read [Why Is Rift Faster?](#why-is-rift-faster) — the flat curve is
the matching architecture, not the language, and the three comparisons above are what separate those
two claims.

The Mountebank suite was run on two deliberately different hosts. Publishing both is the point:
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

The usual answer is "it is written in Rust", and that is genuinely a large part of it: native code,
no GC pauses showing up in the tail, a work-stealing async runtime, and parsing that avoids copying.
Those are real and they are why each individual request is cheap.

But it is only half the answer, and it is the half that always gets said. The other half is the
**matching architecture** — how Rift decides *which* stub answers a request — and that is what
governs the shape of the curve: whether throughput holds as your stub count grows, or slides. Rift's
matcher is an index, not a scan.

The two compound rather than compete. The architecture decides how much work a request costs at all;
the implementation decides how fast that work runs. Every number on this page is the product of both,
and the same design in a slower runtime, or the same runtime over a linear scan, would give up a
different part of the result.

Microcks is a useful check on the distinction. It is *also* indexed rather than scanning, so it is
also flat by stub position — which is why the gap against it is mostly per-request cost rather than
scaling. Mountebank and WireMock do scan, so against them the gap *widens* with stub count. Same
engine, two different shapes, for two different reasons.

The architecture is the part that is usually left out, so it goes first.

### The matching architecture

Naively, "does this request match any of my stubs" is a linear scan: evaluate every stub's
predicates until one matches. That is what Mountebank does, and it is why its per-request cost tracks
stub count. Rift splits it in two
([`crates/rift-mock-core/src/imposter/core/`](https://github.com/achird-labs/rift/tree/master/crates/rift-mock-core/src/imposter/core)):

**Stage 1 — a candidate prefilter** (`stub_index.rs`) narrows thousands of stubs to the handful that
*could* match. **Stage 2 — full predicate evaluation** (`matching.rs`) is the unchanged Mountebank
semantics, and remains the single source of truth. Stage 1 only ever decides what Stage 2 does not
need to look at.

#### Dimensions and a bitset intersection

The index is a set of independent **dimensions** — six of them, over four request attributes (the
path carries three, one each for exact, literal and regex constraints). Each answers one question:
*which stubs can this attribute not rule out?* The answers are **dense bitsets over stub
ids**, and a stub's id is its position in the declaration-ordered stub vector — so a candidate set is
a fixed-width bitset rather than a list of indices. `candidates()` ANDs the per-dimension bitsets and
walks the surviving bits in ascending order.

This is the **Lucent bit-vector technique from packet classification**, applied to stub matching:
each dimension prunes independently, and the intersection is the candidate set. Three consequences
worth stating:

- **Ascending bit order *is* Mountebank's first-match-wins order**, so *ordering* is a property of
  the data structure rather than something a test has to cover. Note the limit of that guarantee: it
  fixes the order of the candidates, not which stubs are in the set — see the soundness rule below.
- **Word-wise `AND` autovectorizes**, and the bitsets are small enough to stay in cache: 4,096 stubs
  is 512 bytes. It is hand-rolled rather than pulling in `roaring`/`fixedbitset` because the only
  operations needed are intersect, union and ascending iteration, and at this scale a dense word
  vector beats a compressed one.
- **Dimensions are concrete struct fields, not `Box<dyn Dimension>`**, so the match loop dispatches
  statically and allocates nothing *extra* for dispatch. (Matching is not allocation-free: the
  accumulator and a per-dimension scratch bitset are allocated per request, and a path containing
  uppercase bytes allocates a folded copy.)

#### The soundness rule

Each dimension's bitset is `matched_bits | always_bits`: stubs whose constraint the request
satisfies, **plus** every stub that either does not constrain this attribute or constrains it in a
shape the dimension cannot index.

> A dimension may only ever exclude a stub it can *prove* cannot match.

So the index is a strict **over-approximation** — `candidates()` returns a superset of the true
matches. That is what makes the whole thing safe in one direction: a dimension that keeps *too many*
stubs costs only performance, so widening a dimension's eligibility later is a pure optimization and
never a semantics question.

The other direction is not safe, and is not pretended to be: a dimension that wrongly *excludes* a
stub makes it silently stop matching. That is the failure this design has to be defended against
rather than reasoned away, so a differential test
(`differential_index_matches_linear_oracle`) runs the index against a linear oracle and fails
immediately on any under-approximation.

#### The six dimensions, and the data structure each uses

Ordered cheapest-first, and the fold **short-circuits as soon as the candidate set is empty** — so a
request that no stub can match usually stops after the first dimension or two:

| # | Dimension | Indexes | Data structure |
|:--|:----------|:--------|:---------------|
| 1 | Method | `equals` on method | **Eight fixed slots** — the seven common verbs plus an "other" bucket, selected by a case-insensitive comparison with no hashing and no allocation |
| 2 | Path (exact) | `equals` on path | Hash map keyed on the case-folded path |
| 3 | Path literals | `startsWith` / `contains` / `endsWith` | **An Aho-Corasick automaton** over every anchor — one pass instead of walking a bucket per anchor (two automata at most: one per case class) |
| 4 | Path regexes | `matches` on path | **A multi-pattern automaton** (`regex-automata`'s meta engine, which picks its own engine per search), reporting every matching pattern id in one overlapping search into a reused thread-local scratch set (again two at most, by case class) |
| 5 | Body (whole) | `deepEquals` on a JSON body | **Structural hash** of the expected body → the stubs sharing it |
| 6 | Body (field) | `equals` on body fields | **A [quamina](https://crates.io/crates/quamina) field automaton** (the Rust port of Tim Bray's design) — "which of N field-equals stubs matches this body" in one pass instead of an O(N) scan |

Two honest asterisks on that table. Dimension 6 is behind the `quamina-matching` cargo feature: it is
**on by default**, but a `default-features = false` build has five dimensions, and those body-field
stubs simply fall through to Stage 2. And dimension 6 *deactivates itself* when no request could ever
reach two body-field stubs at once — if every such stub owns a distinct `(path, method)`, the earlier
dimensions have already done the work, and building the automaton would be pure overhead (worth ~6.5%
throughput to skip).

The two body dimensions run last because they are the only ones that touch the request body, so the
cheap path and method dimensions get to empty the accumulator first; and the field automaton runs
only if the hash dimension left survivors. The body is parsed as JSON **once per request** and shared
across both.

A dimension that indexes nothing is skipped entirely rather than paying a full-width copy and
intersect to learn nothing — so an imposter whose stubs the index cannot help with degrades to the
plain scan instead of paying for an index it is not using.

#### Why regex stopped being the exception

Regex used to be Rift's own slow path: it cannot be hash-dispatched, so at the 100th pattern Rift
managed ~54k RPS. Dimension 4 replaced a per-pattern loop with a single multi-pattern automaton, and
regex now runs at ~207k RPS, in line with every other predicate type — not a micro-optimization but a
change of complexity class, from "one search per pattern" to "one search".

### The implementation stack

The other half, and the reason the index's savings actually show up in the numbers rather than being
absorbed by per-request overhead. It is also most of what separates Rift from a similarly-indexed
engine like Microcks.

| Aspect | Mountebank | Rift |
|:-------|:-----------|:-----|
| **Language** | Node.js (JavaScript) | Rust |
| **Concurrency** | Single-threaded event loop | Multi-threaded (Tokio, work-stealing) |
| **Memory model** | Garbage collected | No GC; per-request allocation avoided on the hot path |
| **Regex engine** | JavaScript `RegExp`, per pattern | `regex-automata`, one multi-pattern automaton |
| **JSON parsing** | `JSON.parse` | `serde_json`, parsed once per request and shared (as are the query map and, lazily, the XML DOM) |
| **Stub matching** | Linear scan | Two-stage: bitset prefilter → full evaluation |
| **Allocator** | — | mimalloc in the server binary (see [below](#memory-allocator-mimalloc)); deliberately *not* in the embedded C-ABI library, which must not impose an allocator on its host |

Plus: native code with no interpreter warm-up, connection reuse to upstream services, and a
dedicated worker pool for script execution so a slow `inject` cannot stall the request path.

### What this does not buy you

Being clear about the limits, since the section above is the optimistic half:

- **The index helps when stubs are distinguishable on an indexed attribute.** An imposter where
  every stub is, say, a body regex indexes on nothing, falls back to the scan, and is as linear as
  Mountebank — just with a much better constant.
- **Stage 2 still runs.** There is nothing for a prefilter to save on a one- or two-stub imposter, so
  the simple-stub row is among the *lowest* multiples in every table on this page (24x vs Mountebank
  on the M4, 4.0x vs WireMock, 21.5x vs Microcks) — the big multiples come from scenarios where the
  index removes work, not from the stack alone. Where something other than matching dominates the
  request — response templating, query-argument dispatch — the multiple can be lower still.
- **None of this is a throughput claim about your workload.** It is why the *curve* is flat; the
  absolute numbers depend on your hardware, your predicates and your response sizes.

---

## Performance Characteristics

### Latency (p99)

| Scenario | Mountebank | Rift |
|:---------|:-----------|:-----|
| Exact stub match (last of 310) | 40ms | 0.6ms |
| Complex AND/OR predicate | 17ms | 0.8ms |
| JSONPath match | 17ms | 1.0ms |
| Regex (100th pattern) | 641ms | 1.8ms |

### Throughput Scaling

Rift maintains consistent throughput regardless of:
- Stub count (310 stubs with minimal degradation)
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
| Deep path match (310 stubs) | 24,264 | 326,779 | **13.5x** | 31.6 ms | 2.5 ms |
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

Rift is roughly flat across the suite — 334k on a trivial stub, 327k on a 310-stub deep path match.
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
| Deep path match (310 stubs) | 417 | 22,867 | 22,846 | 183,847 | **8.0x** |
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

---

## Rift vs Microcks

[Microcks](https://microcks.io/) is the Apache-2.0, CNCF-incubating alternative. Rift is
**13.6x–54.6x** its throughput on the HTTP matching path, with a p99 of ~2.4 ms against 75–250 ms:

| Scenario | Microcks | Rift | Rift/Microcks | Microcks p99 | Rift p99 |
|:---------|---------:|-----:|--------------:|-------------:|---------:|
| Simple static stub | 16,192 | 347,604 | **21.5x** | 78.9 ms | 2.3 ms |
| API first stub (1st of 310) | 6,457 | 338,592 | **52.4x** | 239.3 ms | 2.4 ms |
| API middle stub | 6,447 | 339,056 | **52.6x** | 249.5 ms | 2.4 ms |
| Deep path match (310 stubs) | 6,420 | 338,404 | **52.7x** | 238.0 ms | 2.4 ms |
| No match | 6,487 | 354,170 | **54.6x** | 166.2 ms | 2.3 ms |
| Query match (last of 100) | 14,397 | 195,966 | **13.6x** | 74.8 ms | 4.0 ms |

<sub>AMD EPYC 7763, 16 vCPU (GitHub `ubuntu-16core`), 2026-07-30. Microcks 1.14.0 on Temurin 21 as a
native JVM; Rift from source. `oha` at 256 keep-alive connections, 20s/scenario after a 10s warmup,
each engine run alone. Median of 3 reps; spread ≤3.3% Microcks, ≤2.7% Rift.</sub>

### The multiple is the least interesting part

Microcks is not a slower mock server so much as a different kind of product — a spec-driven mocking
*and contract testing* platform with a web UI, multi-tenancy, a datastore and eight protocols. Raw
stub-serving throughput is not what it was built for. Quoting 54x without that is misleading.

**The result that matters is that Microcks does *not* pay per candidate stub.** Its three API points
sit within **0.6%** of each other across first, middle and last of the same 310 operations — it
resolves by path and verb rather than scanning, which is the same architectural property described in
[the matching architecture](#the-matching-architecture) above. Compare WireMock, which loses **69%**
across that same interval.

| Interval | Microcks | Rift | WireMock |
|:---------|---------:|-----:|---------:|
| Trivial stub → 310 stubs | −60% | **−3%** | −71% |
| First → last of the same 310 | **−0.6%** | **−0.06%** | −69% |

So "throughput stays flat" means something different in each comparison: against Mountebank and
WireMock it is about *scan behaviour*, and against Microcks — which does not scan either — it is
about per-request cost. That is the distinction
[Why Is Rift Faster?](#why-is-rift-faster) opens with, and Microcks is what makes it visible.

Two limits, because the first row above is doing two things at once. The trivial-stub point is a
*different service* with a smaller `text/plain` response, so that −60% mixes corpus size with
payload — the confound-free number is the −0.6%. And the WireMock column comes from the 2026-07-27
run on an Intel Xeon 8573C while Microcks and Rift are from 2026-07-30 on an AMD EPYC 7763 (GitHub's
`ubuntu-16core` pool is heterogeneous; Rift itself reads 334k vs 347k across them), so each column is
sound read downwards and the cross-column absolutes are not.

Full methodology, the stock-defaults column, where Microcks is genuinely better, and what the data
does *not* support: [Rift vs Microcks]({{ site.baseurl }}/comparisons/microcks/).

---

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

**Microcks** is measured in [Rift vs Microcks](#rift-vs-microcks) above and written up in full at
[Rift vs Microcks]({{ site.baseurl }}/comparisons/microcks/). It is deliberately *not* a row in the
table above: its figures come from a different dispatch on different hardware, and this table's whole
value is that every cell in it came from one run on one host.

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
rift --runtime work-stealing

# Per-core: N single-threaded runtimes, N = physical cores
rift --runtime per-core

# …or pin the worker count explicitly
rift --runtime per-core=8

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
