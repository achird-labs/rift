# Rift vs Mountebank Performance Benchmark

Compares [Rift](https://github.com/EtaCassiopeia/rift) (Rust) against
[Mountebank](http://www.mbtest.org/) (Node.js) on byte-identical imposter
configs. Two harnesses, both native processes (no Docker):

- **`bench_direct.py`** — request *serving* throughput and tail latency.
- **`bench_admin.py`** — the admin control plane: creating an imposter with many
  stubs and reading it back (where Rift's stub-overlap analysis, issue #423, lives).

## Prerequisites

```bash
cargo build --release -p rift-http-proxy          # build Rift from this repo
cargo install oha                                 # load generator
npm install --prefix ~/bench-mb mountebank@2.9.1  # reference engine
# python3 is used to orchestrate the runs
```

> `oha` initialises a TLS stack that reads the macOS keychain even for plain-HTTP
> targets — run these outside a restricted sandbox.

## Running

```bash
cd tests/benchmark

# Serving throughput (13 scenarios, ~10 min)
python3 scripts/bench_direct.py --run-all \
    --duration 20s --warmup 3s --connections 50 \
    --rift-bin ../../target/release/rift-http-proxy \
    --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb
cat results/DIRECT_BENCHMARK_REPORT.md

# Admin create/read
python3 scripts/bench_admin.py --run-all \
    --rift-bin ../../target/release/rift-http-proxy \
    --mb-bin ~/bench-mb/node_modules/mountebank/bin/mb
cat results/ADMIN_BENCHMARK_REPORT.md
```

The default `--run-all` is the Rift-vs-Mountebank comparison and is unchanged
(`results/DIRECT_BENCHMARK_REPORT.md`). `direct_rift.csv`/`direct_mb.csv` now carry
three extra columns — `connections`, `mode` (`closed` or `open@<rate>`), and `p999_ms`.
Readers key off the header (`csv.DictReader`), so the added columns don't break parsing;
the Turbo-round modes below reuse the same schema.

### Turbo round: concurrency sweep, recording, and open-loop (Rift-only)

These modes measure *Rift's* scaling and tail behaviour; they force `--engines rift`
(the Mountebank comparison stays the single-point run above). Output is
`results/DIRECT_RIFT_SWEEP_REPORT.md` (a scenario × connection matrix of RPS and p999)
plus the extended `direct_rift.csv`.

**Step 1 — sweep to find saturation.** Run every scenario across a range of connection
counts and read where RPS stops climbing:

```bash
python3 scripts/bench_direct.py --run-all --engines rift \
    --sweep-connections 1,10,50,200 \
    --rift-bin ../../target/release/rift-http-proxy
cat results/DIRECT_RIFT_SWEEP_REPORT.md
```

The sweep also runs the **`recording_on`** scenario — the `api_middle` stub set on an
imposter with `recordRequests: true`, so the journal write path is under load. After each
point the harness asserts the journal recorded requests and stayed within the 10,000-entry
cap (at any point above a trickle of traffic it fills to that cap); its row is marked
`**(recording)**` in the report.

**Step 2 — open-loop at fractions of saturation.** Closed-loop hides tail latency
(coordinated omission). Take the saturation RPS `S` from the sweep and re-run at a *fixed
arrival rate* (`oha -q`) of 50 % / 80 % / 95 % of `S` to see the real tail:

```bash
# e.g. saturation S ≈ 200000 → run at 100000, 160000, 190000
for rate in 100000 160000 190000; do
  python3 scripts/bench_direct.py --run-all --engines rift \
      --open-loop $rate --connections 200 \
      --rift-bin ../../target/release/rift-http-proxy
  cp results/DIRECT_RIFT_SWEEP_REPORT.md results/open_loop_$rate.md
done
```

Compare the `p999_ms` rows across the three fractions: a tail that stays flat up to 95 %
of `S` and only then climbs is healthy; one that climbs at 50 % points at backpressure or
accept-loop contention — exactly the structural changes the Turbo Tier-3/Tier-4 issues
target.

### Allocator bake-off (issue #717, Rift-only)

`--allocator {mimalloc,jemalloc,system}` benches one allocator variant: it builds the binary
with the matching feature set into its own `target/alloc-<name>/` (so the three builds coexist;
an explicit `--rift-bin` skips the build and is trusted verbatim), samples the engine's RSS once
a second (`rss_mb_peak`/`rss_mb_end` CSV columns + an RSS matrix in the report), and writes
suffixed artefacts (`direct_rift_<name>.csv`, `DIRECT_RIFT_SWEEP_REPORT_<name>.md`) so runs
never overwrite each other:

```bash
for alloc in mimalloc jemalloc system; do
  python3 scripts/bench_direct.py --run-all --allocator $alloc \
      --sweep-connections 50,200 --duration 15s --warmup 2s
done
```

The three variants differ **only** in the allocator — `redis-backend`+`javascript` stay enabled
in all of them — and each binary logs `Global allocator: <name>` at startup, so a report can
never mislabel its build. Decision rule and result recording live in #717 (pre-registered: a
default switch needs ≥5% RPS or ≥20% p999/RSS on the majority of scenarios, macOS numbers are
indicative — the decision run is Linux x86_64).

### Runtime topology sweep (issue #746, Rift-only, Linux)

`--runtime {work-stealing,per-core}` benches one topology and composes with `--allocator`
(artefacts get a combined suffix, e.g. `direct_rift_per-core.csv`). A probe launch checks the
binary's `Runtime topology:` self-report first — on macOS a requested `per-core` falls back to
work-stealing by design (RFC-712 D5), so the probe **aborts rather than mislabel the sweep**;
run per-core sweeps on Linux. Per-worker accept counts are exported as
`rift_accepted_connections_total{worker=…}` for skew evidence:

```bash
for rt in work-stealing per-core; do
  python3 scripts/bench_direct.py --run-all --runtime $rt \
      --sweep-connections 1,64,256,512 --duration 15s --warmup 2s
done
```

#### Core-count axis (the RFC-712 slope clause)

Connections alone do not test RFC-712's thesis, which is about the *slope* of RPS vs cores.
`--server-cores N` adds that axis: it confines the engine to N CPUs with `taskset`, and confines
`oha` to the remaining **physical** cores. Two properties make the comparison honest:

- **Both topologies size their workers from N.** Per-core and tokio's work-stealing pool both
  derive their count from `available_parallelism()`, which honours `sched_getaffinity` — so one
  `taskset` sizes them identically, and the probe asserts per-core self-reports `per-core xN`
  (a mismatch means the pinning never reached the engine, and the run aborts).
- **The generator never shares a core with the engine.** The split falls on physical-core
  boundaries, so `oha` cannot land on the SMT sibling of a core under measurement — contention
  that otherwise reads as a scaling ceiling. A budget that splits a hyperthread pair, or that
  leaves the generator no cores, is rejected with the host's valid budgets.

```bash
for n in 2 4 8; do
  for rt in work-stealing per-core; do
    python3 scripts/bench_direct.py --run-all --runtime $rt --server-cores $n \
        --sweep-connections 256,512 --duration 12s --warmup 2s
  done
done   # -> direct_rift_{work-stealing,per-core}_cores{2,4,8}.csv
```

Linux only (`taskset`/`lscpu`). Note the ceiling this implies: on an M-vCPU box the generator
needs its own cores, so the engine tops out well below M — an ≥8-*physical*-core point needs a
bigger box or an off-box generator, and any verdict quoting these numbers should say so.

Both scripts run each engine **one at a time on disjoint port ranges** (no CPU
contention, no cross-talk), launch it in its own process group and hard-kill it by
group + `lsof` before the next engine starts, and post **identical** imposter JSON to
both. Every serving scenario sends one real request first and asserts the returned
**body** — a fall-through to the empty no-match default aborts the run, so a
mis-configured stub can't silently inflate throughput.

Outputs land in `results/` and are gitignored (machine-specific — regenerate per box).

## Latest results

Native processes, unconstrained, on **Apple M4 (10 cores) / macOS**. Rift `0.1.0`
@ `f029cf8`, Mountebank `2.9.1`, `oha` at 50 keep-alive connections, 20s/scenario
after a 3s warmup. Fixture: 10 imposters, 862 stubs.

### Request serving

| Scenario | Mountebank (RPS) | Rift (RPS) | Speedup | p99 MB → Rift (ms) |
|---|--:|--:|--:|---|
| simple_health | 4,093 | 199,228 | **49x** | 1502* → 0.7 |
| api_first | 8,124 | 209,555 | **26x** | 2.9 → 0.5 |
| api_middle | 3,071 | 198,504 | **65x** | 44.1 → 0.6 |
| api_last | 1,351 | 201,403 | **149x** | 40.3 → 0.6 |
| no_match (404) | 1,309 | 206,865 | **158x** | 53.1 → 0.5 |
| regex_last | 106 | 54,434 | **515x** | 640.9 → 1.8 |
| complex_and_or | 4,646 | 181,320 | **39x** | 17.0 → 0.8 |
| json_body_equals | 7,802 | 188,247 | **24x** | 9.1 → 0.8 |
| jsonpath | 4,480 | 173,671 | **39x** | 17.0 → 1.0 |
| xpath | 5,552 | 174,567 | **31x** | 15.1 → 0.8 |
| template | 9,446 | 189,649 | **20x** | 7.2 → 0.6 |
| header_route | 3,050 | 148,739 | **49x** | 34.5 → 0.8 |
| query_param | 2,366 | 118,228 | **50x** | 49.9 → 1.1 |

<sub>*Mountebank's `simple_health` p99 spike is a Node GC pause during the run; its median was 1.8ms.</sub>

### Admin create/read

Fresh engine per (shape, N); create = `POST /imposters` with N stubs, GET = median
of 5 reads, RSS via `ps`. `identical` = every stub shares one predicate (the O(n²)
case #423 fixed); `distinct` = the cheap control. Rift's `warnings` are its
stub-overlap analysis, a Rift extension Mountebank does not perform.

| Shape | N | Create MB → Rift (ms) | GET MB → Rift (ms) | RSS Δ MB → Rift (MB) | Rift warnings |
|---|--:|---|---|---|--:|
| identical | 100 | 16.1 → 9.5 | 4.7 → 1.6 | 6.9 → 2.3 | 99 |
| identical | 1000 | 114.7 → 6.6 | 6.6 → 2.5 | 51.1 → 9.1 | 101 |
| distinct | 100 | 13.8 → 2.3 | 2.1 → 0.3 | 6.0 → 2.2 | 0 |
| distinct | 1000 | 134.9 → 5.3 | 8.6 → 1.4 | 50.3 → 9.5 | 0 |

### Key findings

1. **Position-independent matching.** Rift holds ~200k RPS whether the matching stub
   is first, middle, or last — and on a no-match 404. Mountebank degrades linearly
   with stub count (8,124 → 1,309 RPS, first → no-match): up to **158x** at the tail.
2. **Regex is the extreme.** 515x (Rift 54k vs MB 106) — Mountebank's per-stub JS
   `RegExp` scan collapses at the 100th pattern. Regex is also Rift's *own* slowest
   matcher (~54k vs ~180k elsewhere), since a regex can't be hash-dispatched.
3. **Structured predicates** (JSONPath, XPath, JSON body, complex AND/OR): **24–39x**.
   Native Rust evaluation stays 174k–188k RPS vs Mountebank's JS 4.5k–7.8k.
4. **Sub-millisecond tail.** Rift p99 stays 0.5–1.8ms across every scenario;
   Mountebank ranges 3–641ms depending on stub count, position, and predicate type.
5. **Admin plane / overlap analysis.** Creating 1,000 fully-overlapping stubs, Rift
   creates in **6.6ms vs Mountebank's 114.7ms** and grows RSS **+9MB vs +51MB**, while
   still computing 101 stub-overlap warnings Mountebank never produces.

## Related

- [Compatibility Tests](../compatibility/) — functional compatibility
- [Integration Tests](../integration/) — integration suite
