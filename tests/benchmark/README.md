# Rift vs Mountebank Performance Benchmark

Compares [Rift](https://github.com/achird-labs/rift) (Rust) against
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

The Microcks suite (`bench_microcks.py`) needs a **JDK 21** and its own jar — see
[Microcks comparison](#microcks-comparison-issue-900).

For the WireMock suite (optional — only needed for `bench_wiremock.py`) you also need a JRE 11+
(use an LTS, 17 or 21) and the standalone jar:

```bash
mkdir -p ~/bench-wiremock && curl -Lo ~/bench-wiremock/wiremock-standalone.jar \
  https://repo1.maven.org/maven2/org/wiremock/wiremock-standalone/3.9.1/wiremock-standalone-3.9.1.jar
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
for rep in 1 2 3; do
  for n in 2 4 8; do
    for rt in work-stealing per-core; do
      python3 scripts/bench_direct.py --run-all --runtime $rt --server-cores $n --rep $rep \
          --sweep-connections 256,512 --duration 12s --warmup 2s
    done
  done
done   # -> direct_rift_{work-stealing,per-core}_cores{2,4,8}_rep{1,2,3}.csv
```

Linux only (`taskset`/`lscpu`). Note the ceiling this implies: on an M-vCPU box the generator
needs its own cores, so the engine tops out well below M — an ≥8-*physical*-core point needs a
bigger box or an off-box generator, and any verdict quoting these numbers should say so.

### WireMock comparison (issue #861)

WireMock is the most widely used JVM mock server, so the published comparison covers both reference
engines people actually migrate from. It lives in its own suite because it cannot consume imposter
JSON — different stub schema, and one port per JVM with admin under `/__admin` on that same port:

```bash
cd tests/benchmark

# Benches WireMock TWICE -> results/direct_wiremock.csv + results/direct_wiremock-stock.csv
python3 scripts/bench_wiremock.py --run-all --duration 20s --warmup 10s --connections 50

# Combine whatever CSVs exist from the SAME box and settings -> WIREMOCK_BENCHMARK_REPORT.md
python3 scripts/bench_wiremock.py --report --connections 50
```

**`--run-all` runs two series and therefore takes roughly twice the wall clock** (issue #865):

| Series | Container threads | Connections | CSV |
|---|---|---|---|
| headline (tuned) | `max(cpu_count, highest connection count)` | all of them (incl. a sweep) | `direct_wiremock.csv` |
| secondary (stock) | WireMock's 10-thread default | `--connections` only | `direct_wiremock-stock.csv` |

WireMock is thread-per-request, so its default 10-thread pool bounds in-flight requests at 10 —
below the 50 connections the comparison offers. Pinning the headline series above the offered
concurrency means the number measures the engine rather than the pool, which is what makes a Rift
win defensible instead of "you throttled WireMock". **The pin is a fairness guarantee, not a
speedup:** on a box with about as many cores as the default pool has threads, the CPU saturates
first and the pin is a no-op — the two WireMock columns in the report tell you whether it bound.

`--container-threads N` overrides the pin, so a thread-sweep curve can be produced by hand. There is
deliberately no standing sweep axis; the suite is already slow. The run records the pin it used
beside the CSV, so a later standalone `--report` states the value actually measured rather than
re-deriving one — if you bench and report in separate invocations you do not need to repeat the flag.

When sweeping, pass a `--connections` value that is one of the swept points: the stock series uses
it alone, and a point outside the swept set would produce rows no report can read (the suite fails
at argument-parse time rather than after benching it).

- **Same fixture, same gates.** The suite *imports* `IMPOSTERS`/`SCENARIOS`/`EXPECT_BODY` from
  `bench_direct` and generates WireMock mappings from them, so the two can never drift. The same 13
  scenarios, oha settings and `verify_body` marker assertion apply, and a non-2xx distribution
  aborts the run — a mistranslated stub falls through to the no-match default and is caught rather
  than published as a fast number.
- **`bench_direct.py` is untouched**, so the rift-vs-mb stability contract is unaffected by
  construction.
- **Engines stay on disjoint ports:** rift +0, mb +100, wiremock +200 (4745–4760), one engine at a
  time.
- **`--warmup 10s`, not 3s.** 3s is thin for a JIT. For a like-for-like table, quote rift/mb numbers
  measured with the same warmup.
- **Response templating stays off** so `${request.path}` is served literally, exactly as Mountebank
  does — otherwise the `template` scenario would compare two different behaviours.
- **The request journal stays off** (`--no-request-journal`). WireMock records every request by
  default, unbounded; Rift and Mountebank are both measured with recording *off*, and journaling is
  a separate additive scenario in the Rift suite because it is a distinct cost. Leaving it on would
  compare WireMock-with-recording against Rift-without, and would also make the four scenarios
  sharing the API instance non-comparable with each other.

Two honest caveats the generated report repeats, because a reader will otherwise over-read the
table:

- **`complex_predicate` is not a pure predicate comparison.** WireMock has no OR across two
  *different* headers within one stub, so those 50 stubs become 101 mappings and the measured
  request matches the 50th candidate where Rift/MB match the 25th — roughly twice the scan. That is
  a genuine cost of expressing this workload in WireMock, but it is not like-for-like.
- **The all-2xx gate is weaker for WireMock.** The catch-all that reproduces Rift/MB's empty-200
  no-match default also means every request returns 200, so the status-distribution check cannot
  detect a fall-through here. The per-scenario body-marker assertion is the gate that does.
- `--report` refuses to combine CSVs whose `connections`/`mode` disagree (stale-artefact guard), and
  renders Mountebank columns as `n/a` when `direct_mb.csv` is absent.

Translator logic is gate-tested in `scripts/test_bench_wiremock.py` (golden mappings per stub
generator, priority ordering, the `or`-split, the catch-all, and a completeness test that runs
*every* stub in the live fixture through the translator so a new generator in `bench_direct.py`
cannot ship silently untranslated). CI runs it — the `Benchmark Scripts` job in `ci.yml` executes
both suites' unit tests on every PR.

#### Publishing the 3-way table (issue #866)

Do not publish a number from one rep, and **do not bolt a WireMock column onto an existing
rift-vs-mb table**. Both are handled by the `Benchmark (publish)` workflow
(`.github/workflows/benchmark-publish.yml`, `workflow_dispatch` only):

```
gh workflow run benchmark-publish.yml \
  -f runner=ubuntu-16core -f reps=3 -f duration=20s -f connections=50 -f warmup=10s
```

It measures **all three engines in one dispatch at one `warmup`**, three reps each, then publishes
medians.

Pass `-f run_sweep=false` when you only want the engine-comparison table. The Rift-only sweep is
roughly three quarters of the wall clock and feeds none of the 3-way numbers, so skipping it turns
a ~3–4 hour dispatch into well under an hour. Leave it on when you are refreshing the published
connection-scaling curve. That single-warmup rule is the point of the input existing: a 3s-warmed Rift against a
10s-warmed JVM is not a comparison, and the rendered table would say nothing about it. The legs run
in a fixed order — comparison, then WireMock, then the Rift-only sweep — because the sweep re-writes
`direct_rift_rep*.csv` at other connection counts; the WireMock leg parks its artefacts under
`results/wiremock/` before that happens. `wiremock_version` and `mb_version` are pinned inputs so a
re-run is reproducible.

The same aggregation runs locally:

```bash
for rep in 1 2 3; do
  python3 scripts/bench_wiremock.py --run-all --rep $rep --connections 50 --warmup 10s
done
# -> direct_<engine>_median.csv for every engine with rep files, then the median table
python3 scripts/bench_wiremock.py --aggregate-reps --report --connections 50
```

`--aggregate-reps` collapses `_repN` CSVs for `wiremock`, `wiremock-stock`, and `rift`/`mb` when
those are present, carrying `reps` and peak-to-peak `rps_spread_pct` into the median CSV. The report
then renders a **Repetition spread** table with each column's `n` in its own header — a single rep
shows `n/a`, never `0.0%`, because peak-to-peak over one sample is zero and the thinnest column
would otherwise read as the steadiest engine.

Four things are hard errors rather than a quieter number:

| Refused | Why |
|---|---|
| a scenario point missing from any rep | the median would rest on fewer samples than the table claims (#773) |
| engines with **different** rep counts | unequal replication favours whichever engine got more samples — the rule `bench_direct` already applies to rift-vs-mb |
| reps with different `--container-threads` | their median describes a configuration nothing measured |
| reps with different `--warmup` | same, and this is the setting the 3-way claim rests on |
| no tuned `wiremock` reps | the headline ratio comes from that series; stock alone cannot stand in |

Both the pin and the warmup are carried onto the `_median` suffix, so the median report states the
values actually used. `--report` prints the **recorded** warmup, not the flag default — passing
`--warmup` to a standalone `--report` overrides it, so omit it unless you mean to.

> The Rift-only sweep now runs at the same `warmup` input as everything else (previously a hardcoded
> `3s`). Sweep figures published from this workflow before that change are not comparable with ones
> produced after it.

### Microcks comparison (issue #900)

[Microcks](https://microcks.io/) is the Apache-2.0, CNCF-incubating alternative a buyer usually
reaches for before a commercial one, so the stub-growth claim needs to hold against it too. It lives
in its own suite for a reason the other two do not share: **Microcks is spec-driven, not
stub-authored.** You do not hand it a stub; you hand it an OpenAPI document and it derives operations
and example responses from that.

```bash
cd tests/benchmark

# One Microcks JVM per imposter, in turn -> results/direct_microcks.csv
python3 scripts/bench_microcks.py --run-all --duration 20s --warmup 10s --connections 50

# Combine whatever CSVs exist from the SAME box and settings -> MICROCKS_BENCHMARK_REPORT.md
python3 scripts/bench_microcks.py --report --connections 50

# Inspect what a given imposter translates to, without launching anything
python3 scripts/bench_microcks.py --emit-spec API | head -40
```

**Prerequisites: JDK 21 and the Microcks app jar.** Microcks 1.14 is Spring Boot 3.5 compiled at
class file version 65, so a Java 17 JRE dies at launch with `UnsupportedClassVersionError`. Maven
Central publishes only non-repackaged jars, so the runnable one is lifted out of the official image —
a *download*, not a deployment, and nothing runs in a container at bench time:

```bash
mkdir -p ~/bench-microcks
cid=$(docker create microcks/microcks-uber:1.14.0)
docker cp "$cid:/deployments/app.jar" ~/bench-microcks/microcks-app.jar
docker rm -f "$cid"
```

Pass `--java /path/to/jdk21/bin/java` if `java` on `PATH` is older.

#### How "N stubs" is translated

For Microcks, *"410 stubs"* is not a thing that exists. The nearest honest analogue is **N matchable
request shapes**, which in OpenAPI means N `path` × `verb` operations — and that is what the
translator emits. Two shapes come out, chosen by the fixture rather than by us:

| Fixture shape | OpenAPI shape | Microcks dispatcher |
|---|---|---|
| distinct literal paths (`Simple`, `API`) | one operation per `path` × `verb`, one response example each | none — path/verb resolution |
| one shared path + query args (`Query`) | one operation, N *named* examples + request-parameter examples under the same names | `URI_PARAMS`, rules `page && size` |

Response bodies are emitted as **raw strings**, not parsed objects. Microcks serves a string example
verbatim but re-serializes an object, and the fixture builds bodies with `json.dumps` (`{"id": 1}`,
with spaces) — so the raw string is what keeps Microcks' bytes identical to Rift's and lets this
suite reuse `EXPECT_BODY`/`verify_body` unchanged. If that ever regresses, the fix is to restore the
raw string, **not** to weaken the marker to a whitespace-insensitive match.

#### Only six of the thirteen scenarios are comparable

The rest are **excluded rather than approximated**, because an approximation publishes a ratio
between two different workloads. `check_scenario_coverage()` makes this a hard error: a scenario added
to `bench_direct.SCENARIOS` fails the run until someone classifies it, so it cannot silently vanish
from a report that claims to cover the suite.

| Measured | Refused, and why |
|---|---|
| `simple_health`, `api_first`, `api_middle`, `api_last`, `no_match`, `query_last` | `regex_last` (no regex path dispatcher), `complex_predicate` / `header_last` (no header dispatcher for REST — only a Groovy SCRIPT, which measures the scripting engine), `json_body_equals` (the 50 stubs sit on distinct paths, so Microcks would resolve on path alone and never read the body), `jsonpath` (different expression dialect), `xpath` (none for REST), `template` (different templating language; a static example would be reported under a templating label) |

#### What we did to keep it fair

Every deviation below moves the number in **Microcks'** favour, which is the safe direction for a
comparison published by Rift. All of them are repeated in the generated report, not just here.

- **Invocation statistics are off** (`mocks.enable-invocation-stats=false`). This one is the
  exception to the sentence above, and it is the most important knob here: Microcks defaults it to
  **on**, counting every mock call and persisting a per-service/per-day/per-hour record (verified —
  500 requests produce `dailyCount: 500` on `/api/metrics/invocations/<service>/<version>`). It is the
  direct analogue of WireMock's request journal, which *is* disabled in its leg, and Rift and
  Mountebank are both measured with recording off. Leaving it on would compare
  Microcks-with-recording against Rift-without — an error that flatters **Rift**, which is the one
  direction that must never be taken quietly.
- **The CORS policy is off** (`mocks.rest.enable-cors-policy=false`). It defaults to on and adds four
  `Access-Control-*` headers to every mock response; neither Rift nor WireMock emits them.
- **Both are published anyway**, in a secondary `microcks-stock` series — same workload, Microcks
  launched exactly as it ships (stats on, CORS on, stock Tomcat pool), benched at the headline
  connection count. It goes in its own CSV under its own engine label so it never enters the headline
  ratio, and the report renders a *Tuned vs stock defaults* table beside it. Same pattern, and the
  same reasoning, as `wiremock-stock` (issue #865): a reader has to be able to tell "Microcks is
  slower" from "Microcks ships with per-request invocation accounting on". `--skip-stock` drops it and
  halves the wall clock. **On a 10-core laptop at 50 connections the two series land within noise of
  each other** (deltas scattered both ways, the stock series sometimes ahead), which is what you would
  expect if the stats write is off the request thread until the CPU saturates — quote the number the
  report actually prints for the host in question rather than this sentence.
- **Tomcat's pool is pinned to `max(cores, connections)`.** Its default is 200 while the published
  table drives 256 — benchmarking that would measure the pool, not the engine. Same fairness argument
  as WireMock's `--container-threads`, and `--tomcat-threads N` overrides it.
- **Heap is pinned** (`-Xms == -Xmx`, default `4g`) rather than left to ergonomic sizing, which is a
  fraction of *host* RAM and would silently differ between a runner and a laptop.
- **One imposter per process.** Microcks multiplexes every service on one port, and its throughput is
  measurably sensitive to *total resident corpus size*, so loading all 14 imposters into one JVM would
  have penalised it against WireMock's per-imposter JVM. Each instance holds exactly the imposter
  under test.
- **AsyncAPI off** (`-Dasync-api.enabled=false`) — background work unrelated to HTTP mocking.
- **Logging at WARN.** Per #718, a per-request log site turns a throughput benchmark into a
  measurement of the logging pipeline. That trap is not Rift-specific.
- **`no_match` is measured as the 404 it genuinely returns.** Rift answers an unmatched request with
  an empty 200 and WireMock is given a catch-all to reproduce that; Microcks has no catch-all
  mechanism. The status gate expects `4xx` *for that scenario only* — relaxing it to "any status"
  would stop it catching a genuinely mis-served stub. The body is empty either way, so the
  `EXPECT_BODY[no_match] is None` assertion still proves nothing matched.
- **In-memory MongoDB.** The `uber` profile keeps everything in an embedded store; a production
  Microcks talks to a real MongoDB over a socket, so this configuration is *faster* than a real
  deployment.
- **Protocol scope.** Microcks also covers AsyncAPI, Kafka, MQTT, AMQP, WebSocket and gRPC, which
  Rift does not. This benchmark is confined to the HTTP matching path where the two overlap and says
  nothing about the rest.

Engines stay on disjoint ports: rift +0, mb +100, wiremock +200, **microcks +300**. Import is
verified by operation count — Microcks returns 201 for an artifact whose parser quietly discarded
operations, and a thinner corpus than Rift's would publish as a faster number.

Translator logic is gate-tested in `scripts/test_bench_microcks.py`, including the byte-identical-body
property, the query-dispatch collapse, the fatality of an untranslatable predicate, and the
`benchmark-publish.yml` leg itself. CI runs it via the same `Benchmark Scripts` job.

#### Publishing just the Rift-vs-Microcks table

Re-measuring Mountebank and both WireMock series to publish a *Microcks* page is waste: the only
column that must come from the same dispatch is **Rift**, because it is the ratio's denominator.
WireMock's figures are already published and are cited rather than re-measured.

```
gh workflow run benchmark-publish.yml \
  -f runner=ubuntu-16core -f reps=3 -f duration=20s -f connections=256 -f warmup=10s \
  -f microcks_only=true
```

`microcks_only=true` skips the Mountebank, WireMock and sweep legs and has the Microcks leg run its
own Rift reps at the same settings — roughly 30 minutes instead of ~2 hours. The WireMock column in
the growth table renders as `—`. `-f run_microcks=false` skips the leg entirely in a full dispatch,
and `-f microcks_version=` pins the version.

### Matching-dimension scenarios (Rift-only, additive)

Several Turbo optimizations had **no benchmark coverage at all** — the suite could not have
detected a regression in them. These scenarios close that, and are kept **separate** from the
13-scenario Mountebank comparison set, which is a stability contract: it must stay comparable with
previously published numbers (enforced by `DefaultRunUnchanged` in the tests). They ride with
Rift-only sweeps, exactly like `recording_on`.

| Scenario | Covers | Was measured before? |
|---|---|---|
| `deepequals_body` | #740 `deepEquals` structural-hash index | no — `deepEquals` appeared nowhere |
| `literal_prefix` / `literal_contains` | #732 anchored/unanchored Aho-Corasick | barely — 1 `startsWith`, 2 `contains` in ~860 stubs |
| `method_mix` | #729 method dimension | no — every scenario was GET or POST |
| `body_field_scale` | #767 quamina body-field automaton | no — see the trap below |

#### The trap `body_field_scale` exists to avoid

`json_body_equals` gives every stub a **unique path**, so the path dimension prunes the candidate
set to one stub *before* the body is consulted. The body-field automaton then re-derives what the
path index already knew. Benchmarking the quamina dimension against it measures **pure overhead**:
run 29738479074 showed −8% at 10, 100 *and* 1000 stubs — flat with N, which is the signature of
measuring a cost with no corresponding benefit.

`body_field_scale` puts N stubs on **one shared path and method**, discriminated only by a body
field, so the `O(N)` structural scan the dimension replaces is actually on the critical path.
Scale it with `--body-field-stubs N`:

```bash
for n in 10 100 1000; do
  for q in on off; do
    python3 scripts/bench_direct.py --run-all --quamina $q --body-field-stubs $n --rep 1 \
        --sweep-connections 256 --duration 12s --warmup 2s
  done
done
```

A test asserts these stubs share exactly one path and one method — if that ever changes, the
scenario silently stops testing the dimension while still appearing to.

### Body-field dimension A/B (issue #779, Rift-only)

`--quamina {on,off}` builds and benches one variant of the quamina-backed body-field candidate
dimension, into `target/quamina-<variant>/` (same discipline as `--allocator`). `--stub-count N`
scales the JSONBody imposter's field-equals-on-body stubs off their default 50 — that count *is*
the axis, because the dimension replaces an `O(N)` scan, so a single stub count measures one
arbitrary point on the curve.

```bash
for n in 10 100 1000; do
  for q in on off; do
    for rep in 1 2 3; do
      python3 scripts/bench_direct.py --run-all --quamina $q --stub-count $n --rep $rep \
          --sweep-connections 256 --duration 12s --warmup 2s
    done
  done
  for q in on off; do
    python3 scripts/bench_direct.py --aggregate-reps "_quamina${q}_stubs${n}"
  done
done
```

**Why the probe matters here more than anywhere else.** The two variants are supposed to return
**identical matching results** — the dimension is a pure over-approximating prefilter, and Stage-2
always decides. So a mislabeled build produces *no* visible symptom: same responses, same status
codes, same journal. Nothing but the label would be wrong. The harness therefore refuses to bench
until the binary's own startup line agrees:

```
INFO rift_http_proxy: Matching dimensions: body-field(quamina)=on
```

That line is the third such self-report, alongside `Global allocator:` (#717) and
`Runtime topology:` (RFC-712), and it exists because issue #777 shipped this dimension enabled in
`rift-mock-core` and compiled out of both the binary and the C-ABI — with CI green throughout,
because the dimension's tests run in the crate where it *was* enabled.

The report also records **binary size**, since the dimension pulls a dependency into the server
binary and into the `cdylib` embedders link into their own process.

#### Repetitions and medians — never quote a single run

**Always pass `--rep N`.** One run of one variant is one sample; a benchmark host is not a
constant. Without `--rep` every repetition writes the *same* filename, so the file left behind is
whichever rep ran last — a canonical-looking artefact holding one unreplicated sample. That is not
hypothetical: it produced a wrong, publicly-retracted number on issue #746, where the last rep
happened to land on a degraded runner ~20% low (issue #773).

With `--rep`, each repetition gets its own `_repN` artefact and nothing is overwritten. Collapse
them into the decision artefact with:

```bash
python3 scripts/bench_direct.py --aggregate-reps "_per-core_cores8"
# -> direct_rift_per-core_cores8_median.csv
#    DIRECT_RIFT_MEDIAN_REPORT_per-core_cores8.md
```

The report carries a **spread** column (peak-to-peak RPS as a percentage of the mean) next to every
median. Read it before quoting a number: a large spread means the reps disagree and the median is
provisional. Aggregation **fails loudly** if a point is missing from any rep, rather than quietly
producing a median backed by fewer samples than the report implies.

`--rep` is Rift-only — the rift-vs-mb comparison report reads unsuffixed artefacts, so a repped
comparison run would report a stale file as the current one.

Both scripts run each engine **one at a time on disjoint port ranges** (no CPU
contention, no cross-talk), launch it in its own process group and hard-kill it by
group + `lsof` before the next engine starts, and post **identical** imposter JSON to
both. Every serving scenario sends one real request first and asserts the returned
**body** — a fall-through to the empty no-match default aborts the run, so a
mis-configured stub can't silently inflate throughput.

Outputs land in `results/` and are gitignored (machine-specific — regenerate per box).

## Latest results

Measured 2026-07-20. Rift built from `master` @ `924cf73`, Mountebank `2.9.1`, `oha`
at 50 keep-alive connections, 20s/scenario after a 3s warmup, native processes
(no Docker), each engine run alone. Fixture: 14 imposters, 1,512 stubs. Every figure
is the **median of 3 repetitions** — reproduce with `--rep 1|2|3` then
`--aggregate-comparison`.

Two hosts, because the multiplier is hardware-dependent:

- **M4** — Apple M4, 10 cores, macOS (laptop)
- **EPYC** — AMD EPYC 9V74, 16 vCPU, 62 GiB, Linux (`benchmark-publish.yml`)

### Request serving

| Scenario | MB (M4) | Rift (M4) | M4 | MB (EPYC) | Rift (EPYC) | EPYC |
|---|--:|--:|--:|--:|--:|--:|
| simple_health | 8,898 | 214,818 | **24x** | 5,982 | 324,952 | **54x** |
| api_first | 8,546 | 211,378 | **25x** | 5,728 | 323,408 | **57x** |
| api_middle | 3,437 | 210,151 | **61x** | 1,081 | 324,067 | **300x** |
| api_last | 1,344 | 209,523 | **156x** | 542 | 322,530 | **595x** |
| no_match (404) | 1,351 | 209,763 | **155x** | 549 | 332,574 | **606x** |
| regex_last | 112 | 207,024 | **1,857x** | 52 | 317,851 | **6,160x** |
| complex_and_or | 4,703 | 191,987 | **41x** | 1,814 | 259,548 | **143x** |
| json_body_equals | 7,611 | 199,670 | **26x** | 2,730 | 294,294 | **108x** |
| jsonpath | 4,312 | 199,404 | **46x** | 1,921 | 304,796 | **159x** |
| xpath | 5,542 | 187,869 | **34x** | 1,966 | 247,897 | **126x** |
| template | 9,022 | 194,236 | **22x** | 3,152 | 283,815 | **90x** |
| header_route | 3,016 | 158,596 | **53x** | 1,202 | 201,940 | **168x** |
| query_param | 2,751 | 164,133 | **60x** | 1,112 | 211,748 | **190x** |

p99 latency, same runs:

| Scenario | p99 MB → Rift, M4 (ms) | p99 MB → Rift, EPYC (ms) |
|---|---|---|
| simple_health | 2.9 → 0.46 | 9.6 → 0.49 |
| api_first | 2.9 → 0.47 | 10.4 → 0.49 |
| api_middle | 46.0 → 0.46 | 51.2 → 0.49 |
| api_last | 40.3 → 0.45 | 114.4 → 0.49 |
| no_match (404) | 40.0 → 0.43 | 96.1 → 0.48 |
| regex_last | 613.9 → 0.46 | 1741.6 → 0.51 |
| complex_and_or | 13.5 → 0.77 | 28.1 → 0.73 |
| json_body_equals | 8.5 → 0.58 | 22.1 → 0.59 |
| jsonpath | 16.2 → 0.54 | 30.3 → 0.56 |
| xpath | 13.0 → 0.70 | 30.0 → 0.75 |
| template | 7.2 → 0.51 | 19.5 → 0.61 |
| header_route | 34.9 → 0.72 | 46.3 → 0.97 |
| query_param | 31.8 → 0.66 | 50.0 → 0.91 |

Reading notes:

- **Rift is faster on EPYC (215k → 325k); Mountebank is *slower* (8,898 → 5,982).**
  Mountebank is single-threaded, and this server's individual cores are slower than
  the M4's, so it gains nothing from the extra 15. The EPYC multipliers are therefore
  inflated at both ends — quote the M4 column when a conservative figure is wanted.
- **`regex_last` is the headline change since the previous run** (54,434 → 207,024 RPS
  on comparable hardware). The candidate-bitset matching framework removed regex as
  Rift's slow path; it is now in line with every other predicate type. Mountebank did
  not change.
- **M4 figures carry ~±10%.** A laptop thermally throttles over a 30-minute run: both
  engines lost ~7% aggregate between the first and last repetition, and per-scenario
  spread reached 12% (versus 5% on EPYC). This is why the table is a median of 3 and
  not a single sample.

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

1. **Position-independent matching.** Rift holds ~210k RPS (M4) / ~325k RPS (EPYC)
   whether the matching stub is first, middle, or last — and on a no-match 404.
   Mountebank degrades linearly with stub count (8,546 → 1,351 RPS, first → no-match):
   up to **155x** at the tail on the M4, **606x** on EPYC.
2. **Regex is no longer Rift's slow path.** It used to be the one predicate type that
   couldn't be hash-dispatched (~54k RPS vs ~180k elsewhere); the candidate-bitset
   matching framework brought it to **207k RPS**, in line with everything else.
   Mountebank's per-stub JS `RegExp` scan still collapses at the 100th pattern, so the
   gap is now **1,857x** (M4) / **6,160x** (EPYC) — widened by Rift improving, not by
   Mountebank regressing.
3. **Structured predicates** (JSONPath, XPath, JSON body, complex AND/OR): **26–46x**
   on the M4, **108–159x** on EPYC. Native Rust evaluation stays 188k–200k RPS (M4)
   vs Mountebank's JS 4.3k–7.6k.
4. **Sub-millisecond tail.** Rift p99 stays **0.43–0.97ms on both hosts**, across every
   scenario; Mountebank ranges from 2.9ms to 1.7 *seconds* depending on stub count,
   position, and predicate type.
5. **Admin plane / overlap analysis.** Creating 1,000 fully-overlapping stubs, Rift
   creates in **6.6ms vs Mountebank's 114.7ms** and grows RSS **+9MB vs +51MB**, while
   still computing 101 stub-overlap warnings Mountebank never produces.

## Related

- [Compatibility Tests](../compatibility/) — functional compatibility
- [Integration Tests](../integration/) — integration suite
