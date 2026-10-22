# Differential conformance: Mountebank is the oracle

Every case here runs on a real Mountebank and on the Rift binary under test, and the only assertion
is that the two answered the same. Nobody writes an expected value: the other engine is the
expectation. This replaces `tests/compatibility/` (issue #1341), which compared status and parsed
body only, never headers and never the stored imposter, and so could not see #1329, #1333 or #1334.

## Running it

```bash
npm install --prefix ~/bench-mb @mbtest/mountebank@2.9.5   # once; the oracle
cargo build -p rift-http-proxy                             # the server binary the harness spawns
cargo test -p rift-differential -- --nocapture
```

`cargo test --workspace` runs it too (it builds the server first). The `differential` job in
`.github/workflows/ci.yml` runs it on every pull request.

| Variable | Effect |
|:---------|:-------|
| `RIFT_MB_BIN` | Mountebank's `mb`. Default: `~/bench-mb/node_modules/.bin/mb`, then `mb` on `PATH` |
| `RIFT_SERVER_BIN` | The Rift server to test. Default: `rift-http-proxy` in the target directory the test was built into, refused when it is older than the engine sources (`cargo test -p rift-differential` does not rebuild it) |
| `RIFT_DIFFERENTIAL_REQUIRE_MB` | Fail, instead of skipping, when no Mountebank is found (CI sets it) |
| `RIFT_DIFFERENTIAL_FILTER` | Run only the cases whose name contains this text (the stale-entry check is then off) |
| `RIFT_DIFFERENTIAL_DUMP` | Write every difference found, explained or not, to this file as JSON — the raw material for a precise allow-list entry |
| `RIFT_DIFFERENTIAL_MB_COVERAGE` | An absolute directory: Mountebank runs under `NODE_V8_COVERAGE` there, and the harness records which Mountebank and node ran in `mountebank.json` beside the profile (see [Mountebank coverage](#mountebank-coverage)) |
| `RIFT_DIFFERENTIAL_MB_COVERAGE_SUMMARY` | c8's `coverage-summary.json` of that profile; `mb_coverage_matches_baseline` holds it against `mb-coverage.json` (skipped when unset) |
| `RIFT_DIFFERENTIAL_MB_COVERAGE_BLESS` | With the summary set: rewrite `mb-coverage.json`'s counts instead of checking them |

Mountebank is pinned to 2.9.5 (`MB_VERSION` in `ci.yml`), the newest stable release on npm and the
version the published benchmark uses (`benchmark-publish.yml`). Since 2.9.2 Mountebank publishes as
`@mbtest/mountebank`; the old `mountebank` package stops at 2.9.1. Betas are not tracked.
`.github/workflows/mountebank-release-watch.yml` opens an issue when a newer stable release ships.

## What is compared

Both engines start once (`--allowInjection --localOnly`, working directory `sdk-conformance/corpus`
so fixture paths like `data/products.csv` resolve). For each case:

1. Every step is sent to both engines. Cases are written with *logical* ports (`4545`); each engine
   gets its own free ports, and every port in a request is mapped forward and every port in an
   answer mapped back, so ports never differ.
2. Every answer is compared: status, every header except `Date`, `Connection`, `Keep-Alive`,
   `Transfer-Encoding`, `Content-Length` and `Server` (names case-insensitive), and the body —
   byte for byte, located by JSON pointer when both bodies are JSON. Admin-API JSON is compared as
   parsed values after canonicalisation (below). A request that gets no answer compares only the
   failure class (connect, timeout, transport).
3. After the last step, every imposter either engine holds is fetched as `GET /imposters/:port` and
   with `?replayable=true`, canonicalised, and diffed.
4. Both engines `DELETE /imposters`.

Canonicalisation (`src/canon.rs`) removes only what is not a contract, at the structural level it
lives at: `_links`; the Rift-only `stubCount`, `enabled` and `_rift`; `_proxyResponseTime`; in a
stub a proxy recorded (Rift's `recordedFrom`, Mountebank's `_proxyResponseTime`), the ignored headers
above and a measured `addWaitBehavior` wait; and everything in a recorded request except `method`,
`path`, `query`, `body`, `headers` and `form` (names case-folded, transport headers dropped). A stub
the case wrote keeps every header it was given, and nothing inside a response body or a predicate is
touched.

Locations name what was compared: `step[3]` is an imposter request, `step[3].admin` an admin call,
`step[3].admin.export` / `.admin.import` a re-import, `step[3].rep[1]` a repeated send,
`stored[4545]` / `replayable[4545]` the final imposters.

## Known differences: `allowlist.json`

A difference fails its case unless an entry in `allowlist.json` names it. An entry is either

- `documented` — a deliberate deviation, with `doc` and a verbatim `quote`. The
  `allow_list_is_valid_and_its_citations_resolve` test fails when the quote is no longer in the doc,
  so deleting the doc line turns the harness red; or
- `known-bug` — a divergence that is not intended, with its `reason` (and `issue` once filed).

`case` names one case, or `*`; `location` is a glob over the difference location (`step[2].body`,
`stored[4545]/stubs/0/responses`) and also covers what lies below it. `mountebank` / `rift` pin a
side's value exactly, `mountebankMatches` / `riftMatches` as an anchored regex, and `relation:
"rift-quotes-mountebank"` ties the two (`200` against `"200"`). A `*` case must pin both values, so
it is a precise claim about what Rift does differently everywhere, never a blanket ignore of a header
or a field. The one other wildcard is the `json-text` class: a served JSON body with the same value,
which Rift serves compact and key-sorted. A difference is credited to the first entry that matches,
and an entry that matches nothing in a full run fails the run, so a fixed divergence cannot leave its
excuse behind.

Case-specific entries pin one leaf each, with both values, so a change in Rift's answer at that spot
is red. Run with `RIFT_DIFFERENTIAL_DUMP` to get the exact locations and values for a new one. Values
that legitimately vary between runs or platforms (an OS-assigned port in a message, `errno`, a
JavaScript stack) are pinned by a regex over their stable part.

The deliberate deviations are listed for users in
[`docs/mountebank/differences.md`](../../docs/mountebank/differences.md).

## Mountebank coverage

The cases are only as good as the Mountebank code they reach, so the suite measures it (#1548):

```bash
scripts/mb-coverage.sh           # run the suite with Mountebank under coverage, report, check
scripts/mb-coverage.sh --bless   # the same, then rewrite the baseline's counts
```

The script runs every `rift-differential` test with `RIFT_DIFFERENTIAL_MB_COVERAGE` set. `mb` is a
`#!/usr/bin/env node` script, so the Mountebank child is node itself and `NODE_V8_COVERAGE` is
enough; after the cases the harness stops Mountebank with `mb stop` (its SIGTERM handler exits
cleanly, which is when node writes the profile; a kill would lose it, and a stop that fails under
coverage fails the run). `c8 report`, run from Mountebank's package directory, writes HTML and
`coverage-summary.json` to `conformance/differential/coverage/` (gitignored; CI uploads it as the
`mb-coverage` artifact). Last, `mb_coverage_matches_baseline` holds the summary against
[`mb-coverage.json`](mb-coverage.json):

- Every file c8 reports under `src/` (less `src/public`) is either `excluded`, with the reason it
  cannot or need not be reached, or has a `baseline` of covered/total lines, branches and
  functions — exactly one of the two. A Mountebank release that adds a file fails until it is
  classified; `--bless` records counts, it never classifies.
- The check is strict both ways. A covered count that drops means a case stopped reaching code it
  used to. One that rises means the baseline is slack: re-bless, so a change that closes a gap shows
  it in the diff.
- Counts, never percentages: totals move with a Mountebank release, and V8 does not count the
  branches of a function that never ran, so a branch percentage can rise while coverage falls.
- The profile must come from the node major the baseline was measured on (`node`, 22, what CI
  pins); V8's block ranges can shift between majors. Another major is refused with a message, not
  compared. To move the major, bless on it and commit the new baseline with the pin change.

The baseline was measured on Mountebank 2.9.5 and node 22.23.3. The in-scope set is 28 files: the
models, `models/http`, `models/https/httpsServer.js`, the imposter, imposters and config
controllers, and `util` less its loggers.

| | lines | branches | functions |
|:--|--:|--:|--:|
| before `coverage.json` | 4268/4867 (87.7%) | 764/924 (82.7%) | 279/306 (91.2%) |
| with it | 4391/4867 (90.2%) | 836/963 (86.8%) | 283/307 (92.2%) |

The branch and function totals grow because code that now runs has branches V8 did not count before.

### Uncovered in-scope code

Line numbers are Mountebank 2.9.5's. Each range is out of reach of a case, or a case for it shows
a divergence (to be fixed in Rift and then covered here), or nobody has written the case yet.

**Divergences — a case would fail today:**

| Mountebank | What a case shows |
|:-----------|:------------------|
| `dryRunValidator.js:81-86`, `responseResolver.js:313-316`, `:341-343`, `imposterController.js:210` with a non-integer index | Response-shape and stub-index validation — tracked by #1550/#1551/#1552 |
| `predicates.js:21-24`, `:238-241`, `:269-271` | Array semantics of `deepEquals`/`equals`: order-independent arrays of objects, an expected array as a subset of the actual one, an object over an array of objects |
| `predicates.js:58-59`, `:338-339` | An imposter with `"mode": "binary"` refuses `matches`, `jsonpath` and `xpath` at creation in Mountebank; Rift creates it |
| `predicates.js:384-389`, `responseResolver.js:69-77` | A predicate or response `inject` that throws: Mountebank answers `500`, Rift `400` |
| `behaviors.js:275-276`, `:364-369` | `copy` from a missing field, and into a header name or body key |
| `baseHttpServer.js:47-50` | A CORS preflight (`Access-Control-Request-Headers` set): Mountebank echoes the request's values and serves the stub's body |
| `baseHttpServer.js:64-65` | A stub's own `Content-Length` header: Mountebank recomputes it from the body |
| `httpRequest.js:63-73` | A `gzip`/`br` `Content-Encoding` whose body does not decode |
| `xpath.js:36-40` | A malformed `xpath` selector: Mountebank creates the imposter and answers `500` per request, Rift refuses it at creation |
| `helpers.js:104-107` | An unknown `fault` |

The `https` cases create their imposters with `POST /imposters` and keep them, so the create answer,
the detail and the replayable export are compared too (#1576). The default certificate's `key` and
`cert`, which Mountebank prints and Rift does not, are allow-listed.

**Out of reach of a case:**

| Mountebank | Why |
|:-----------|:----|
| `imposterController.js:124-128`, `:140-146`; `imposter.js:79-98`, `:132-138`; `responseResolver.js:281-295`, `:357-359`, `:376-393`; `dryRunValidator.js:91-93`; `httpProxy.js:62-68`; `middleware.js:125-127` | The callback API of out-of-process (`--protofile`) protocols, which Rift does not implement |
| `dryRunValidator.js:130-171`, `:229-232` | The injection checks, which run only without `--allowInjection`; the harness runs both engines with it |
| `dryRunValidator.js:250-251` | A protocol's own validator; http and https have none |
| `responseResolver.js:136-137` | `buildExists` recursing into an object: every `exists` generator crashes on the dry run's second key first (#1580), so no imposter that would reach it can be created |
| `compatibility.js:91-100`; `imposterPrinter.js:31-32` | tcp-only upcasts and printing |
| `imposter.js:105-107`, `baseHttpServer.js:135-137`, `ip.js:31-47` | `--ipWhitelist` blocking (#879: not enforced in Rift) |
| `imposter.js:126-127`, `inMemoryImpostersRepository.js:82-89` | `--debug` match recording; the harness starts both engines without it |
| `middleware.js:228-252` | `--apikey`; likewise |
| `imposter.js:195-196`; `impostersRepository.js:21-39` | `--host` and `--impostersRepository`/`--datadir` |
| `middleware.js:146-153`, `:173-176`; `date.js:32-45` | HTML page rendering, Internet Explorer's `Accept` header, and the relative dates of the log and feed pages |
| `httpProxy.js:81-85` | `http_proxy`/`https_proxy` in Mountebank's environment |
| `httpProxy.js:118-119` | A proxied body with neither `Content-Length` nor `Transfer-Encoding`, which an HTTP/1.1 client cannot send |
| `baseHttpServer.js:77-95` | `CONNECT` tunnelling, which the harness's client cannot send as a plain request |
| `baseHttpServer.js:107`, `:161-165`, `:179`; `imposter.js:238-239`; `impostersController.js:207-208` | Socket errors, a bind the OS refuses (the `hold` step's `127.0.0.1` listener does not stop a wildcard bind), and a connection still open at shutdown |
| `inMemoryImpostersRepository.js:174-175`, `:189-190`, `:357-359` | Guarded by the controllers' own index checks; imposters still running when Mountebank stops (every case deletes its imposters) |

**Not yet written** (reachable, no case yet): `behaviors.js:346-348`, `:453-454`, `:472-474`,
`:507-508`; `behaviorsValidator.js:23-24`, `:40-41`, `:107-108`, `:160-162`; `httpProxy.js:133-134`,
`:203-204`, `:215-217`; `jsonpath.js:27-28`; `predicates.js:51-53`, `:137-138`, `:396-398`;
`responseResolver.js:86-87`; `xpath.js:15-16`, `:26-28`; `helpers.js:58-59`, `:122-127` (an array or
object `ignore`).

## Generated cases

`tests/generative.rs` (issue #1549) generates cases instead of reading them. It draws random
imposters and request/admin sequences, runs each on both engines through the same
`driver::run_case` as a committed case, and shrinks any unexplained difference to a minimal case.
Stubs and requests come from the model test's generators
(`crates/rift-mock-core/tests/model/ops.rs`); `tests/generative/vocab.rs` keeps the part
Mountebank can run.

```bash
cargo test -p rift-differential --test generative -- --nocapture             # the PR smoke
RIFT_GEN_SEED=7 RIFT_GEN_SECONDS=600 cargo test -p rift-differential --test generative -- --nocapture
```

| Variable | Effect |
|:---------|:-------|
| `RIFT_GEN_SEED` | The seed (default `1549`, so every PR runs the same cases; the nightly uses the run id). Batch `n` of a timed run uses `seed + n`, and a failure names the batch seed that reproduces it |
| `RIFT_GEN_CASES` | Cases per batch (default 32) |
| `RIFT_GEN_SECONDS` | Run batches until this much time has passed, instead of one batch (the nightly sets 1200) |
| `RIFT_GEN_MAX_STEPS_PER_SEC` | The pacing cap (default 150). Every step opens about 2.6 fresh connections (the client never pools, so a fault cannot poison the next request), and each leaves a `TIME_WAIT` socket; 150 steps/s stays under Linux's ~470 connections/s sustained ceiling |
| `RIFT_GEN_OUT` | Where a failure is written. Default: `<target>/generative/` |

Every PR runs one batch on the fixed seed inside the `differential` job (`cargo test -p
rift-differential` runs every test target), with Mountebank unprofiled, so the coverage ratchet
measures the committed cases only. The `generative` job of `.github/workflows/model-tests.yml`
runs it nightly for 20 minutes.

**Vocabulary.** Requests: `GET`/`POST` to `/a` or `/b`, `?q=1` or not, a body `x`/`y` on a `POST`.
Stubs: `equals`, `deepEquals`, `contains`, `startsWith`, `matches`, `exists` and `not` over
`method`, `path`, `query` and `body`; `is` responses with `repeat` 1–3, a `copy` from the path or
query into the body and a `wait` of 0; `proxyOnce`/`proxyAlways` to an upstream imposter (4546)
with a single-key `predicateGenerators`. Operations: a request, `POST`/`DELETE`/`PUT
/imposters/4545/stubs/:i` (indices up to one past the end), `PUT /imposters/4545/stubs`, `PUT
/imposters` (both body orders), delete-and-recreate, `DELETE .../savedProxyResponses` and `DELETE
.../savedRequests`. Every admin operation is followed by `GET /imposters/4545`.

**Exclusions.** Each one names its excuse in `vocab::EXCLUDED`, and a unit test checks that the
cited allow-list id or doc quote still exists, so an exclusion cannot outlive its reason:

| Not generated | Why |
|:--------------|:----|
| `predicateGenerators.matches` with more than one key | Rift's recorded predicate order is documented, but allow-listed only at the indices of `proxy: predicateGenerators with method and headers` |
| `inject`, `decorate`, `shellTransform` | JavaScript and shell run differently by design (`docs/mountebank/differences.md`) |
| scenarios, `_rift.conditional`, flow state, reload, stub move | Rift-only: Mountebank has nothing to compare them with, and the model test covers them |
| the model's `X-Flow` header, a non-zero `wait` | noise, or time without coverage |

**Allow-list.** Generated cases are named `generated: …`, so only the Rift-wide (`"case": "*"`)
entries apply, and a generated run never fails because an entry went unused. No regex-wide entry is
added for a generated location: one would quietly accept a real bug next to it. The vocabulary
stays out of a shape instead, and says so in `EXCLUDED`.

**A failure** is shrunk, then re-run three times. It is a *divergence* when every re-run fails at
the same locations, otherwise *flaky*. The case file is written to `<seed>.json`, named
`generated: <sig>`, where `<sig>` is the de-indexed, sorted locations hashed to 8 hex digits: the
key the nightly de-duplicates on (`gen-sig:<sig>` in an open `agent-found` issue gets a comment
instead of a new issue). An infrastructure error (port allocation, an unreadable imposter list, a
failed cleanup, neither engine accepting a connection) stops the run without shrinking and is never
reported as a divergence. `verdict.json` records which of the three it was.

**Promoting a finding.** Once triaged, paste the case from `<seed>.json` into
`cases/generated.json` (a case file like the others), with a `known-bug` allow-list entry naming it
until the fix lands. The committed case then runs on every PR with no generator involved.

## Cases

`cases/*.json`, one file per source:

- `admin_api.json` … `stub_management.json` — the request and imposter shapes of the 11 retired
  `tests/compatibility/features/*.feature` files, one case per scenario. Assertions were dropped:
  the oracle replaces them.
- `sdk-corpus.json` — the Mountebank-drivable fixtures of `sdk-conformance/corpus/imposters/`,
  loaded from the corpus itself, `_verify` stripped, each `_verify.sequence[].request` sent in order.
- `targeted.json` — the miss classes the retired suite could not see (#1329, #1333, #1334,
  `deepEquals` on query), and one case per documented deviation the allow-list cites.
- `coverage.json` — one case per Mountebank code path the rest did not reach (#1548), named
  `coverage: <mb file>:<line> <what>` after the line it first reaches in Mountebank 2.9.5.
- `generated.json` — shrunk cases found by the generator (see [Generated cases](#generated-cases)),
  named `generated: <sig>`, kept as regressions.

A step is `{"do": "admin", "method", "path", "body"?}`, `{"do": "send", "port", "scheme"?, "method"?,
"path"?, "headers"?: [[name, value]], "body"? | "bodyOfSize"?, "times"?}` (`"scheme": "https"`
reaches an `https` imposter over TLS, accepting its self-signed certificate; the default is `http`), `{"do": "concurrent", ...,
"times"}` (the answers are compared as a multiset), `{"do": "reimport"}` (each engine re-imports
its own `?replayable=true` export), or `{"do": "hold", "port"}` (the harness itself listens on
127.0.0.1 at each engine's mapped port until the case ends, standing in for another process). A string `body` is sent verbatim, any other JSON value serialised.

### SDK-corpus fixtures not driven

| Fixture | Why |
|:--------|:----|
| 04, 17 | `_rift.fault` responses |
| 06 | `_rift` flow state and Rhai scripts |
| 10 | `space` (Rift-only) |
| 11 | `{{...}}` response templating (Rift-only) |
| 20 | `_rift` and Rift-only aliases |

Single stubs dropped from driven fixtures are listed with their reason in `cases/sdk-corpus.json`
(`dropStubs`): Mountebank refuses the whole imposter for each of them.

### Retired scenarios not ported here

The retired suite's `@rift-only` and `@skip` scenarios ran against Rift alone; there is no oracle for
them. Each is either covered elsewhere or ported to
`crates/rift-http-proxy/tests/compat_rift_only.rs`:

| Retired scenario(s) | Now |
|:--------------------|:----|
| `debug_mode.feature` (all 9) | `compat_rift_only.rs` `debug_mode_*` |
| `proxy.feature` pathRewrite (3) | `compat_rift_only.rs` `path_rewrite_*` |
| `admin_api.feature` get all stubs / stub by index, and the three stub-read 404 scenarios | `compat_rift_only.rs` `get_all_stubs_for_an_imposter`, `get_stub_by_index`, `stub_reads_answer_404_for_a_missing_imposter_or_index` (Mountebank has no `GET .../stubs` route) |
| `rift_extensions.feature` script validation (7) | `compat_rift_only.rs`; the Lua and missing-`should_inject` scenarios assert today's behaviour (#450, #453) |
| `rift_extensions.feature` error fault with headers, empty `_rift`, per-client header lookup | `compat_rift_only.rs` |
| `stub_management.feature` shadowed / duplicate-id warnings | `compat_rift_only.rs` `warnings_for_*` |
| `stub_management.feature` stub id accepted | `crates/rift-http-proxy/tests/admin_api_integration.rs` (stub by id) |
| `mountebank_compatibility_gaps.feature` keyCaseSensitive, `host` field | Mountebank runs them: `targeted.json` |
| `rift_extensions.feature` flow state, latency and error faults, `_behaviors` + fault, predicates / cycling / default response with `_rift`, plain config | `crates/rift-http-proxy/tests/rift_extensions.rs` |
| `rift_extensions.feature` circuit breaker, token bucket, retry, session affinity, cascading failure, idempotency, A/B, feature flag, saga (2), health aggregation, coalescing, failover | Not ported: each is JavaScript logic over the inject + flow-state mechanism `rift_extensions.rs` and corpus fixtures 05/06 already cover; none exercises engine behaviour of its own |
| `alternative_formats.feature` `rules` alias, predicates-over-rules, `delayRange`, `recordedFrom` | `crates/rift-mock-core/src/imposter/types.rs` unit tests and corpus fixture 20 |
| `responses.feature` shellTransform (2, one `@skip`) | `behaviors/transform.rs` unit tests, `issue_1198_behavior_program.rs`; the served difference is pinned by `targeted.json` |
